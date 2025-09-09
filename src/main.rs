use anyhow::{Context, Result, anyhow};

use drm::Device as DrmDevice;
use drm::buffer::{Buffer, DrmFourcc};
use drm::control as ctrl;
use drm::control::dumbbuffer::DumbBuffer;
use drm::control::{Device as CtrlDevice, Mode, PageFlipFlags, connector, crtc, framebuffer};
use evdev::{Device as EvDev, EventSummary, KeyCode};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::io::{AsFd, BorrowedFd};

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
    disp_w: usize,
    disp_h: usize,
    stride: usize,
}

struct Surface {
    card: Card,
    con: connector::Handle,
    crtc: crtc::Handle,
    mode: Mode,
    disp_w: u32,
    disp_h: u32,
    frames: [Frame; 2],
    front: usize,
    flipping: bool,
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
            let mut db = card.create_dumb_buffer((disp_w, disp_h), fmt, 32)?;

            {
                let mut map = card.map_dumb_buffer(&mut db)?;

                for px in map.as_mut().chunks_exact_mut(4) {
                    px[0] = 128;
                    px[1] = 128;
                    px[2] = 128;
                    px[3] = 0;
                }
            }

            let fb = card.add_framebuffer(&db, 24, 32)?;

            let stride = db.pitch();

            Ok(Frame {
                db,
                fb,
                disp_w: disp_w as usize,
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
            con,
            crtc,
            mode,
            disp_w,
            disp_h,
            frames: [f0, f1],
            front: 0,
            flipping: false,
        })
    }

    #[inline]
    fn back(&self) -> usize {
        1 - self.front
    }

    fn write_to_back_bytes(&mut self, src: &[u8], src_stride_bytes: usize) -> Result<()> {
        let frame = &mut self.frames[self.back()];

        assert!(
            src_stride_bytes >= frame.stride,
            "source stride is less than framebuffer stride"
        );
        assert!(
            src.len() >= src_stride_bytes * frame.disp_h,
            "source buffer is too small"
        );

        let mut map = self.card.map_dumb_buffer(&mut frame.db)?;

        for y in 0..frame.disp_h {
            let src_0 = y * src_stride_bytes;
            let dst_0 = y * frame.stride;
            let src_row = &src[src_0..src_0 + src_stride_bytes];
            let dst_row = &mut map[dst_0..dst_0 + frame.stride];
            dst_row.copy_from_slice(src_row);
        }

        Ok(())
    }

    fn flip(&mut self) -> Result<()> {
        assert!(!self.flipping, "flip already pending");

        let target_frame = &self.frames[self.back()];

        self.card
            .page_flip(self.crtc, target_frame.fb, PageFlipFlags::EVENT, None)?;

        self.flipping = true;

        Ok(())
    }

    fn handle_drm_events(&mut self) -> Result<()> {
        for event in self.card.receive_events()? {
            match event {
                ctrl::Event::PageFlip(_) => {
                    if self.flipping {
                        self.front = self.back();
                        self.flipping = false;
                    }
                }
                _ => {}
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
enum Pattern {
    Solid,
    Gradient,
    Checker,
    Motion,
}

const SOLIDS: &[(u8, u8, u8)] = &[
    (255, 0, 0),
    (0, 255, 0),
    (0, 0, 255),
    (255, 255, 255),
    (128, 128, 128),
    (0, 0, 0),
];

fn put_rgb(buf: &mut [u8], pitch: usize, x: usize, y: usize, r: u8, g: u8, b: u8) {
    let offset = y * pitch + x * 4;

    assert!(offset + 3 < buf.len(), "put_rgb out of bounds");

    buf[offset + 0] = b;
    buf[offset + 1] = g;
    buf[offset + 2] = r;
    buf[offset + 3] = 0xff;
}

fn fill_rgb(buf: &mut [u8], pitch: usize, w: usize, h: usize, r: u8, g: u8, b: u8) {
    for y in 0..h {
        for x in 0..w {
            put_rgb(buf, pitch, x, y, r, g, b);
        }
    }
}

fn open_keyboard() -> Result<EvDev> {
    for (path, dev) in evdev::enumerate() {
        if dev
            .supported_keys()
            .map_or(false, |keys| keys.contains(KeyCode::KEY_SPACE))
        {
            dev.set_nonblocking(true)?;

            eprintln!("Using keyboard: {}, Name: {:?}", path.display(), dev.name());

            return Ok(dev);
        }
    }
    Err(anyhow!("can't find device"))
}

fn main() -> Result<()> {
    let mut surface = Surface::open_default()?;

    let mut kb = open_keyboard()?;

    let mut red_on = false;

    eprintln!("Press 'Space' to toggle, 'Q' to quit.");

    let ww = surface.disp_w as usize;
    let hh = surface.disp_h as usize;
    let stage_pitch = ww * 4;

    let mut stage = vec![0u8; hh * stage_pitch];

    fill_rgb(&mut stage, stage_pitch, ww, hh, 255u8, 0u8, 0u8);

    surface.write_to_back_bytes(&stage, stage_pitch)?;
    surface.flip()?;

    let drm_file = unsafe { File::from_raw_fd(surface.card.as_fd().as_raw_fd()) };
    let kb_file = unsafe { File::from_raw_fd(kb.as_raw_fd()) };

    let drm_fd = drm_file.as_fd();
    let kb_fd = kb_file.as_fd();

    let mut fds = [
        PollFd::new(drm_fd, PollFlags::POLLIN),
        PollFd::new(kb_fd, PollFlags::POLLIN),
    ];

    'mainloop: loop {
        let _ = poll(&mut fds, 1u16)?;

        if fds[0]
            .revents()
            .unwrap_or(PollFlags::empty())
            .contains(PollFlags::POLLIN)
        {
            surface.handle_drm_events()?;
        }

        if fds[1]
            .revents()
            .unwrap_or(PollFlags::empty())
            .contains(PollFlags::POLLIN)
        {
            if let Ok(events) = kb.fetch_events() {
                for event in events {
                    eprintln!("event?");
                    match event.destructure() {
                        EventSummary::Key(_, KeyCode::KEY_SPACE, 1) => {
                            eprintln!("Key press detected");

                            red_on = !red_on;

                            if red_on {
                                fill_rgb(&mut stage, stage_pitch, ww, hh, 255, 0, 0);
                                surface.write_to_back_bytes(&stage, stage_pitch)?;
                            } else {
                                fill_rgb(&mut stage, stage_pitch, ww, hh, 128, 128, 128);
                                surface.write_to_back_bytes(&stage, stage_pitch)?;
                            }

                            surface.flip()?;
                        }
                        EventSummary::Key(_, KeyCode::KEY_Q, 1) => {
                            break 'mainloop;
                        }
                        _ => {
                            continue;
                        }
                    }
                }
            }
        }

        eprintln!("blocked?");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    Ok(())
}
