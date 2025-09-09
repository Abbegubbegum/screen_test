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

    fn handle_drm_events(&mut self) -> Result<()> {
        for event in self.card.receive_events()? {
            if let ctrl::Event::PageFlip(_) = event {
                if self.is_flipping {
                    self.front = self.back();
                    self.is_flipping = false;
                }
            }
        }

        Ok(())
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

#[derive(Clone, Copy, Debug)]
enum PatternKind {
    Solid,
    Gradient,
    Checker,
    Motion,
}

#[derive(Clone, Copy, Debug)]
enum GradMode {
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

    assert!(offset + 3 < buf.len(), "put_rgb out of bounds");

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
    fill_rgb(buf, stride, w, h, 0, 0, 0);

    let x0 = x_pos.min(w);
    let x1 = (x_pos + bar_w).min(w);

    for y in 0..h {
        for x in x0..x1 {
            put_rgb(buf, stride, x, y, 255, 255, 255);
        }
    }
}

fn overlay_near_patches(buf: &mut [u8], stride: usize, w: usize, h: usize) {
    let sz = (w.min(h) / 10).max(32);

    for (i, v) in (1u8..=5u8).enumerate() {
        for y in (i * sz / 5)..((i + 1) * sz / 5) {
            for x in 0..sz {
                put_rgb(buf, stride, x, y, v, v, v);
            }
        }
    }

    for (i, v) in (250u8..=254u8).enumerate() {
        let y0 = h.saturating_sub(sz) + (i * sz / 5);
        for y in y0..(y0 + sz / 5).min(h) {
            for x in (w.saturating_sub(sz))..w {
                put_rgb(buf, stride, x, y, v, v, v);
            }
        }
    }
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

struct AppState {
    patterns: [PatternKind; 3],
    pattern_idx: usize,
    solid_idx: usize,
    grad_mode: GradMode,
    grad_vertical: bool,
    checker_cell: usize,
    motion_x: isize,
    motion_speed: usize,
    motion_dir: i32,
    show_patches: bool,
    last_switch: Instant,
}

impl AppState {
    fn new() -> Self {
        Self {
            patterns: [
                PatternKind::Solid,
                PatternKind::Gradient,
                PatternKind::Checker,
            ],
            pattern_idx: 0,
            solid_idx: 0,
            grad_mode: GradMode::Luma,
            grad_vertical: false,
            checker_cell: 8,
            motion_x: 0,
            motion_speed: 8,
            motion_dir: 1,
            show_patches: false,
            last_switch: Instant::now(),
        }
    }

    fn next_pattern(&mut self) {
        self.pattern_idx = (self.pattern_idx + 1) % self.patterns.len();
        self.last_switch = Instant::now();
    }

    fn previous_pattern(&mut self) {
        self.pattern_idx = (self.pattern_idx + self.patterns.len() - 1) % self.patterns.len()
    }

    fn pattern(&self) -> PatternKind {
        self.patterns[self.pattern_idx]
    }

    fn next_gradmode(&mut self) {
        self.grad_mode = match self.grad_mode {
            GradMode::Luma => GradMode::Red,
            GradMode::Red => GradMode::Green,
            GradMode::Green => GradMode::Blue,
            GradMode::Blue => GradMode::Luma,
        }
    }

    fn increment_cellsize(&mut self) {
        self.checker_cell = match self.checker_cell {
            1 => 2,
            2 => 4,
            4 => 8,
            8 => 16,
            16 => 32,
            _ => 1,
        }
    }
}

fn main() -> Result<()> {
    let mut surface = Surface::open_default()?;

    let mut kb = open_keyboard()?;

    let mut stage = vec![0u8; surface.disp_h * surface.stride()];

    let mut state = AppState::new();
    let mut need_redraw = true;

    surface.write_to_back(&stage)?;
    surface.flip()?;

    let mut last_frame = Instant::now();

    'mainloop: loop {
        let (drm_ready, kb_ready) = {
            let mut fds = [
                PollFd::new(surface.card.as_fd(), PollFlags::POLLIN),
                PollFd::new(kb.as_fd(), PollFlags::POLLIN),
            ];

            let _ = poll(&mut fds, 1u16)?;

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
            surface.handle_drm_events()?;
        }

        if kb_ready {
            if let Ok(events) = kb.fetch_events() {
                for event in events {
                    if let EventSummary::Key(_, code, 1) = event.destructure() {
                        match code {
                            KeyCode::KEY_Q | KeyCode::KEY_ESC => break 'mainloop,
                            KeyCode::KEY_RIGHT => {
                                state.next_pattern();
                            }
                            KeyCode::KEY_LEFT => {
                                state.previous_pattern();
                            }
                            KeyCode::KEY_SPACE => match state.pattern() {
                                PatternKind::Solid => {
                                    state.solid_idx = (state.solid_idx + 1) % SOLIDS.len();
                                }
                                PatternKind::Gradient => {
                                    state.next_gradmode();
                                }
                                PatternKind::Checker => {
                                    state.increment_cellsize();
                                }
                                PatternKind::Motion => {
                                    state.motion_dir *= -1;
                                }
                            },
                            KeyCode::KEY_V => {
                                state.grad_vertical = !state.grad_vertical;
                            }
                            KeyCode::KEY_P => {
                                state.show_patches = !state.show_patches;
                            }
                            KeyCode::KEY_M => {
                                state.motion_speed = match state.motion_speed {
                                    2 => 4,
                                    4 => 8,
                                    8 => 16,
                                    16 => 32,
                                    _ => 2,
                                }
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

        if need_redraw || matches!(state.pattern(), PatternKind::Motion) {
            match state.pattern() {
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
            }

            if state.show_patches {
                overlay_near_patches(&mut stage, surface.stride(), surface.disp_w, surface.disp_h);
            }
        }

        let should_submit = need_redraw || matches!(state.pattern(), PatternKind::Motion);
        if should_submit && !surface.is_flipping {
            surface.write_to_back(&stage)?;
            surface.flip()?;
            need_redraw = false;
        }
    }

    Ok(())
}
