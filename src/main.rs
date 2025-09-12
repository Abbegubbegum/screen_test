use anyhow::{Context, Result, anyhow, ensure};

use drm::Device as DrmDevice;
use drm::buffer::{Buffer, DrmFourcc};
use drm::control as ctrl;
use drm::control::dumbbuffer::DumbBuffer;
use drm::control::{Device as CtrlDevice, PageFlipFlags, connector, crtc, framebuffer};
use evdev::{Device as EvDev, EventSummary, KeyCode};
use std::fs::{File, OpenOptions};
use std::os::unix::io::{AsFd, BorrowedFd};
use std::time::Instant;

use nix::poll::{PollFd, PollFlags, poll};

#[derive(Debug)]
struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl DrmDevice for Card {}

impl CtrlDevice for Card {}

impl Card {
    fn open_default() -> Self {
        let mut options = OpenOptions::new();
        options.read(true).write(true);

        Card(options.open("/dev/dri/card0").unwrap())
    }
}

struct Frame {
    db: DumbBuffer,
    fb: framebuffer::Handle,
    _disp_w: usize,
    disp_h: usize,
    stride: usize,
}

struct Surface {
    card: Card,
    crtc: crtc::Handle,
    disp_w: usize,
    disp_h: usize,
    frames: [Frame; 2],
    front: usize,
    is_flipping: bool,
}

impl Surface {
    fn open_default() -> Result<Self> {
        let card = Card::open_default();

        let res = card
            .resource_handles()
            .context("could not load resource handles")?;

        let mut selected = None;

        for &con in res.connectors() {
            let info = card.get_connector(con, false)?;
            if info.state() != connector::State::Connected || info.modes().is_empty() {
                continue;
            }

            let mode = info
                .modes()
                .iter()
                .find(|m| m.mode_type().contains(ctrl::ModeTypeFlags::PREFERRED))
                .or_else(|| info.modes().get(0))
                .cloned()
                .ok_or_else(|| anyhow!("connector has no modes"))?;

            let enc = info
                .current_encoder()
                .ok_or_else(|| anyhow!("no current encoder"))?;

            let enc_info = card.get_encoder(enc)?;

            let crtc = enc_info.crtc().ok_or_else(|| anyhow!("no crtc"))?;

            selected = Some((con, crtc, mode));
            break;
        }

        let (con, crtc, mode) = selected.ok_or_else(|| anyhow!("no connected display"))?;

        let (disp_w, disp_h) = (mode.size().0 as u32, mode.size().1 as u32);

        let fmt = DrmFourcc::Xrgb8888;

        let make_frame = || -> Result<Frame> {
            let db = card.create_dumb_buffer((disp_w, disp_h), fmt, 32)?;

            let fb = card.add_framebuffer(&db, 24, 32)?;

            let stride = db.pitch();

            Ok(Frame {
                db,
                fb,
                _disp_w: disp_w as usize,
                disp_h: disp_h as usize,
                stride: stride as usize,
            })
        };

        let f0 = make_frame()?;
        let f1 = make_frame()?;

        card.set_crtc(crtc, Some(f0.fb), (0, 0), &[con], Some(mode))
            .context("failed to set crtc")?;

        Ok(Self {
            card,
            crtc,
            disp_w: disp_w as usize,
            disp_h: disp_h as usize,
            frames: [f0, f1],
            front: 0,
            is_flipping: false,
        })
    }

    #[inline]
    fn back(&self) -> usize {
        1 - self.front
    }

    #[inline]
    fn stride(&self) -> usize {
        self.frames[0].stride
    }

    fn write_to_back(&mut self, src: &[u8]) -> Result<()> {
        let frame = &mut self.frames[self.back()];
        ensure!(
            src.len() >= frame.stride * frame.disp_h,
            "source buffer too small"
        );

        let mut map = self.card.map_dumb_buffer(&mut frame.db)?;

        for y in 0..frame.disp_h {
            let p0 = y * frame.stride;
            let p1 = p0 + frame.stride;
            let src_row = &src[p0..p1]; // If the row is longer i.e src_strice > frame.stride, only copy up to frame.stride pixels
            let dst_row = &mut map[p0..p1];
            dst_row.copy_from_slice(src_row);
        }

        Ok(())
    }

    fn flip(&mut self) -> Result<()> {
        ensure!(!self.is_flipping, "flip already pending");

        let target_frame = &self.frames[self.back()];

        self.card
            .page_flip(self.crtc, target_frame.fb, PageFlipFlags::EVENT, None)?;

        self.is_flipping = true;

        Ok(())
    }

    fn handle_drm_events(&mut self) -> Result<bool> {
        for event in self.card.receive_events()? {
            if let ctrl::Event::PageFlip(_) = event {
                if self.is_flipping {
                    self.front = self.back();
                    self.is_flipping = false;
                    return Ok(true);
                }
            }
        }

        Ok(false)
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        let _ = self.card.set_crtc(self.crtc, None, (0, 0), &[], None);
        for f in &self.frames {
            let _ = self.card.destroy_framebuffer(f.fb);
            let _ = self.card.destroy_dumb_buffer(f.db);
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
enum PatternKind {
    #[default]
    Solid,
    Gradient,
    Checker,
    Motion,
    Patches,
    Viewing,
}

#[derive(Clone, Copy, Debug, Default)]
enum GradMode {
    #[default]
    Luma,
    Red,
    Green,
    Blue,
}

const SOLIDS: &[(u8, u8, u8)] = &[
    (255, 0, 0),
    (0, 255, 0),
    (0, 0, 255),
    (255, 255, 255),
    (128, 128, 128),
    (0, 0, 0),
];

fn put_rgb(buf: &mut [u8], stride: usize, x: usize, y: usize, r: u8, g: u8, b: u8) {
    let offset = y * stride + x * 4;

    assert!(offset + 3 < buf.len(), "put_rgb out of bounds {}, {}", x, y);

    buf[offset + 0] = b;
    buf[offset + 1] = g;
    buf[offset + 2] = r;
    buf[offset + 3] = 0xff;
}

fn fill_rgb(buf: &mut [u8], stride: usize, w: usize, h: usize, r: u8, g: u8, b: u8) {
    for y in 0..h {
        for x in 0..w {
            put_rgb(buf, stride, x, y, r, g, b);
        }
    }
}

fn draw_gradient(
    buf: &mut [u8],
    stride: usize,
    w: usize,
    h: usize,
    mode: GradMode,
    vertical: bool,
) {
    match mode {
        GradMode::Luma => {
            let len = if vertical { h } else { w };
            for y in 0..h {
                for x in 0..w {
                    let t = if vertical { y } else { x };
                    let v = ((t * 255) / (len - 1).max(1)) as u8;
                    put_rgb(buf, stride, x, y, v, v, v);
                }
            }
        }
        _ => {
            let channel = match mode {
                GradMode::Red => 0,
                GradMode::Green => 1,
                GradMode::Blue => 2,
                _ => unreachable!(),
            };

            let len = if vertical { h } else { w };
            for y in 0..h {
                for x in 0..w {
                    let t = if vertical { y } else { x };
                    let v = ((t * 255) / (len - 1).max(1)) as u8;
                    let (mut r, mut g, mut b) = (0u8, 0u8, 0u8);

                    match channel {
                        0 => r = v,
                        1 => g = v,
                        2 => b = v,
                        _ => {}
                    };

                    put_rgb(buf, stride, x, y, r, g, b);
                }
            }
        }
    }
}

fn draw_checkerboard(buf: &mut [u8], stride: usize, w: usize, h: usize, cell: usize) {
    let cell = cell.max(1);

    for y in 0..h {
        let by = (y / cell) & 1;
        for x in 0..w {
            let bx = (x / cell) & 1;
            let white = (bx ^ by) == 0;
            let v = if white { 255 } else { 0 };
            put_rgb(buf, stride, x, y, v, v, v);
        }
    }
}

fn draw_motion_bar(buf: &mut [u8], stride: usize, w: usize, h: usize, x_pos: usize, bar_w: usize) {
    fill_rgb(buf, stride, w, h, 128, 128, 128);

    let x0 = x_pos.min(w.saturating_sub(1));
    let x1 = (x_pos + bar_w).min(w);

    for y in 0..h {
        for x in x0..x1 {
            put_rgb(buf, stride, x, y, 230, 230, 230);
        }
    }
}

fn draw_patches(buf: &mut [u8], stride: usize, w: usize, h: usize) {
    fill_rgb(buf, stride, w, h, 32, 32, 32);

    let colors_area = (w.min(h) / 5).max(64);
    let color_area = colors_area / 5;

    for (i, v) in (1u8..=5u8).enumerate() {
        let y0 = i * color_area;

        for y in y0..((i + 1) * color_area) {
            for x in 0..colors_area.min(w) {
                put_rgb(buf, stride, x, y, v, v, v);
            }
        }
    }

    for (i, v) in (250u8..=254u8).enumerate() {
        let y0 = h.saturating_sub(colors_area) + (i * color_area);

        for y in y0..((i + 1) + color_area).min(h.saturating_sub(1)) {
            for x in (w.saturating_sub(colors_area))..w {
                put_rgb(buf, stride, x, y, v, v, v);
            }
        }
    }
}

fn clamp_rect(
    x: isize,
    y: isize,
    w: usize,
    h: usize,
    ww: usize,
    hh: usize,
) -> (usize, usize, usize, usize) {
    let mut x0 = x.max(0) as usize;
    let mut y0 = y.max(0) as usize;
    let mut rw = w.min(ww.saturating_sub(x0));
    let mut rh = h.min(hh.saturating_sub(y0));
    if x0 >= ww {
        x0 = 0;
        rw = 0;
    }
    if y0 >= hh {
        y0 = 0;
        rh = 0;
    }
    (x0, y0, rw, rh)
}
fn fill_rect(
    buf: &mut [u8],
    stride: usize,
    ww: usize,
    hh: usize,
    x: isize,
    y: isize,
    w: usize,
    h: usize,
    r: u8,
    g: u8,
    b: u8,
) {
    let (x0, y0, rw, rh) = clamp_rect(x, y, w, h, ww, hh);
    for yy in y0..y0 + rh {
        for xx in x0..x0 + rw {
            put_rgb(buf, stride, xx, yy, r, g, b);
        }
    }
}
fn draw_rect_outline(
    buf: &mut [u8],
    stride: usize,
    ww: usize,
    hh: usize,
    x: isize,
    y: isize,
    w: usize,
    h: usize,
    t: usize,
    r: u8,
    g: u8,
    b: u8,
) {
    fill_rect(buf, stride, ww, hh, x, y, w, t, r, g, b);
    fill_rect(
        buf,
        stride,
        ww,
        hh,
        x,
        y as isize + (h as isize - t as isize),
        w,
        t,
        r,
        g,
        b,
    );
    fill_rect(buf, stride, ww, hh, x, y, t, h, r, g, b);
    fill_rect(
        buf,
        stride,
        ww,
        hh,
        x as isize + (w as isize - t as isize),
        y,
        t,
        h,
        r,
        g,
        b,
    );
}
fn draw_crosshair(buf: &mut [u8], stride: usize, w: usize, h: usize, r: u8, g: u8, b: u8) {
    let cx = w / 2;
    let cy = h / 2;
    // horizontal line
    for x in 0..w {
        put_rgb(buf, stride, x, cy, r, g, b);
    }
    // vertical line
    for y in 0..h {
        put_rgb(buf, stride, cx, y, r, g, b);
    }
}
fn draw_viewing_card(buf: &mut [u8], stride: usize, w: usize, h: usize) {
    // black background
    fill_rgb(buf, stride, w, h, 0, 0, 0);

    // white border
    let t = (w.min(h) / 200).max(2);
    draw_rect_outline(buf, stride, w, h, 0, 0, w, h, t, 255, 255, 255);

    // corner boxes with high-contrast content
    let box_w = (w / 5).max(80);
    let box_h = (h / 5).max(80);
    // TL: white box
    fill_rect(
        buf,
        stride,
        w,
        h,
        t as isize * 2,
        t as isize * 2,
        box_w,
        box_h,
        255,
        255,
        255,
    );
    // TR: fine checker (tests scaling / chroma)
    let cell = (box_w / 12).max(2);
    for yy in 0..box_h {
        for xx in 0..box_w {
            let on = ((xx / cell + yy / cell) & 1) == 0;
            let v = if on { 255 } else { 0 };
            put_rgb(buf, stride, w - box_w - t * 2 + xx, t * 2 + yy, v, v, v);
        }
    }
    // BL: vertical color bars (R,G,B)
    let seg = (box_w / 3).max(8);
    fill_rect(
        buf,
        stride,
        w,
        h,
        t as isize * 2,
        (h - box_h - t * 2) as isize,
        seg,
        box_h,
        255,
        0,
        0,
    );
    fill_rect(
        buf,
        stride,
        w,
        h,
        (t * 2 + seg) as isize,
        (h - box_h - t * 2) as isize,
        seg,
        box_h,
        0,
        255,
        0,
    );
    fill_rect(
        buf,
        stride,
        w,
        h,
        (t * 2 + 2 * seg) as isize,
        (h - box_h - t * 2) as isize,
        seg,
        box_h,
        0,
        0,
        255,
    );
    // BR: diagonal stripes (luminance)
    for yy in 0..box_h {
        for xx in 0..box_w {
            let v = if ((xx + yy) / 8) % 2 == 0 { 220 } else { 30 };
            put_rgb(
                buf,
                stride,
                w - box_w - t * 2 + xx,
                h - box_h - t * 2 + yy,
                v,
                v,
                v,
            );
        }
    }

    // center crosshair
    draw_crosshair(buf, stride, w, h, 255, 255, 0);
}

fn open_keyboard() -> Result<EvDev> {
    for (path, dev) in evdev::enumerate() {
        if dev
            .supported_keys()
            .map_or(false, |keys| keys.contains(KeyCode::KEY_SPACE))
        {
            eprintln!("Using keyboard: {}, Name: {:?}", path.display(), dev.name());

            return Ok(dev);
        }
    }
    Err(anyhow!("can't find device"))
}

#[derive(Clone, Copy, Default)]
struct Step {
    pat: PatternKind,
    solid_idx: usize,
    grad_mode: GradMode,
    grad_vertical: bool,
    checker_cell: usize,
    motion_speed: usize,
}

struct AppState {
    pattern: PatternKind,
    solid_idx: usize,
    grad_mode: GradMode,
    grad_vertical: bool,
    checker_cell: usize,
    motion_x: isize,
    motion_speed: usize,
    motion_dir: i32,

    script: Vec<Step>,
    script_idx: usize,
}

impl AppState {
    fn new() -> Self {
        let script = AppState::create_script();

        let mut appstate = Self {
            pattern: PatternKind::Solid,
            solid_idx: 0,
            grad_mode: GradMode::Luma,
            grad_vertical: false,
            checker_cell: 8,
            motion_x: 0,
            motion_speed: 8,
            motion_dir: 1,
            script,
            script_idx: 0,
        };

        appstate.apply_current_step();

        return appstate;
    }

    fn create_script() -> Vec<Step> {
        let mut script = Vec::new();

        for i in 0..SOLIDS.len() {
            script.push(Step {
                pat: PatternKind::Solid,
                solid_idx: i,
                ..Default::default()
            })
        }

        script.push(Step {
            pat: PatternKind::Gradient,
            grad_mode: GradMode::Luma,
            grad_vertical: false,
            ..Default::default()
        });

        script.push(Step {
            pat: PatternKind::Gradient,
            grad_mode: GradMode::Luma,
            grad_vertical: true,
            ..Default::default()
        });

        for &gm in &[GradMode::Red, GradMode::Green, GradMode::Blue] {
            script.push(Step {
                pat: PatternKind::Gradient,
                grad_mode: gm,
                grad_vertical: false,
                ..Default::default()
            });
        }

        for &c in &[16, 8, 4, 2] {
            script.push(Step {
                pat: PatternKind::Checker,
                checker_cell: c,
                ..Default::default()
            });
        }

        for &s in &[4, 8, 16] {
            script.push(Step {
                pat: PatternKind::Motion,
                motion_speed: s,
                ..Default::default()
            });
        }

        script.push(Step {
            pat: PatternKind::Patches,
            ..Default::default()
        });

        script.push(Step {
            pat: PatternKind::Viewing,
            ..Default::default()
        });

        return script;
    }

    fn current_step(&self) -> Step {
        self.script[self.script_idx]
    }

    fn apply_current_step(&mut self) {
        let step = self.current_step();
        self.pattern = step.pat;
        self.solid_idx = step.solid_idx;
        self.grad_mode = step.grad_mode;
        self.grad_vertical = step.grad_vertical;
        self.checker_cell = step.checker_cell;
        self.motion_speed = step.motion_speed;
        self.motion_x = 0;
        self.motion_dir = 1;
    }

    // Returns if program should quit
    fn next_step(&mut self) -> bool {
        self.script_idx += 1;

        if self.script_idx > self.script.len() {
            return true;
        }

        self.apply_current_step();

        return false;
    }

    fn previous_step(&mut self) {
        self.script_idx = (self.script_idx + self.script.len() - 1) % self.script.len();
        self.apply_current_step();
    }
}

fn main() -> Result<()> {
    let mut surface = Surface::open_default()?;

    let mut kb = open_keyboard()?;

    let mut stage = vec![0u8; surface.disp_h * surface.stride()];

    let mut state = AppState::new();

    surface.write_to_back(&stage)?;
    surface.flip()?;

    let mut last_frame = Instant::now();

    let mut need_redraw = true;

    let mut pause = false;

    'mainloop: loop {
        let (drm_ready, kb_ready) = {
            let mut fds = [
                PollFd::new(surface.card.as_fd(), PollFlags::POLLIN),
                PollFd::new(kb.as_fd(), PollFlags::POLLIN),
            ];

            let _ = poll(&mut fds, 30u16)?;

            let drm_ready = fds[0]
                .revents()
                .unwrap_or(PollFlags::empty())
                .contains(PollFlags::POLLIN);

            let kb_ready = fds[1]
                .revents()
                .unwrap_or(PollFlags::empty())
                .contains(PollFlags::POLLIN);

            (drm_ready, kb_ready)
        };

        if drm_ready {
            println!("flip has gone through!");
            surface.handle_drm_events()?;
        }

        if kb_ready {
            if let Ok(events) = kb.fetch_events() {
                for event in events {
                    if let EventSummary::Key(_, code, 1) = event.destructure() {
                        match code {
                            KeyCode::KEY_Q | KeyCode::KEY_ESC => break 'mainloop,
                            KeyCode::KEY_RIGHT | KeyCode::KEY_SPACE => {
                                if state.next_step() {
                                    break 'mainloop;
                                }
                            }
                            KeyCode::KEY_LEFT => {
                                state.previous_step();
                            }
                            KeyCode::KEY_V => {
                                state.grad_vertical = !state.grad_vertical;
                            }
                            KeyCode::KEY_M => {
                                state.motion_speed = match state.motion_speed {
                                    1 => 2,
                                    2 => 4,
                                    4 => 8,
                                    8 => 16,
                                    16 => 32,
                                    _ => 1,
                                }
                            }
                            KeyCode::KEY_P => {
                                pause = !pause;
                            }
                            _ => {}
                        }

                        need_redraw = true;
                    }
                }
            }
        }

        let now = Instant::now();
        let _dt = now.duration_since(last_frame);
        last_frame = now;

        if pause {
            continue;
        }

        let should_draw = need_redraw || matches!(state.pattern, PatternKind::Motion);

        if should_draw {
            println!("draw stage");

            match state.pattern {
                PatternKind::Solid => {
                    let (r, g, b) = SOLIDS[state.solid_idx];

                    fill_rgb(
                        &mut stage,
                        surface.stride(),
                        surface.disp_w,
                        surface.disp_h,
                        r,
                        g,
                        b,
                    );
                }
                PatternKind::Gradient => {
                    draw_gradient(
                        &mut stage,
                        surface.stride(),
                        surface.disp_w,
                        surface.disp_h,
                        state.grad_mode,
                        state.grad_vertical,
                    );
                }
                PatternKind::Checker => {
                    draw_checkerboard(
                        &mut stage,
                        surface.stride(),
                        surface.disp_w,
                        surface.disp_h,
                        state.checker_cell,
                    );
                }
                PatternKind::Motion => {
                    let bar_w = (surface.disp_w / 40).max(8);
                    state.motion_x = state.motion_x
                        + (state.motion_dir as isize) * (state.motion_speed as isize);

                    if state.motion_x < 0 {
                        state.motion_x = (surface.disp_w as isize) - 1
                    } else if state.motion_x as usize >= surface.disp_w {
                        state.motion_x = 0;
                    }

                    draw_motion_bar(
                        &mut stage,
                        surface.stride(),
                        surface.disp_w,
                        surface.disp_h,
                        state.motion_x as usize,
                        bar_w,
                    );
                }
                PatternKind::Patches => {
                    draw_patches(&mut stage, surface.stride(), surface.disp_w, surface.disp_h);
                }
                PatternKind::Viewing => {
                    draw_viewing_card(&mut stage, surface.stride(), surface.disp_w, surface.disp_h);
                }
            }
        }

        if should_draw && !surface.is_flipping {
            println!("draw");
            surface.write_to_back(&stage)?;
            surface.flip()?;

            need_redraw = false;
        }

        println!("loop");
    }

    Ok(())
}
