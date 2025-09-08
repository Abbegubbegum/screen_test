use anyhow::{Context, Result, anyhow};

use drm::Device as DrmDevice;
use drm::buffer::{Buffer, DrmFourcc};
use drm::control as ctrl;
use drm::control::dumbbuffer::{DumbBuffer, DumbMapping};
use drm::control::{Device as CtrlDevice, Mode, connector, crtc, framebuffer};
use memmap2::MmapMut;
use std::fs::{File, OpenOptions};
use std::os::unix::io::{AsFd, BorrowedFd};

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

struct Frame<'a> {
    dbuf: DumbBuffer,
    fb: framebuffer::Handle,
    map: DumbMapping<'a>,
    stride: u32,
}

struct Surface<'a> {
    card: Card,
    con: connector::Handle,
    crtc: crtc::Handle,
    mode: Mode,
    w: u32,
    h: u32,
    frames: [Frame<'a>; 2],
    front: usize,
}

impl Surface<'_> {
    fn open_default() -> Result<()> {
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

        let (display_width, display_height) = mode.size();

        let fmt = DrmFourcc::Xrgb8888;

        let mut make_solid_fb = |r: u8,
                                 g: u8,
                                 b: u8|
         -> Result<(DumbBuffer, framebuffer::Handle)> {
            let mut db =
                card.create_dumb_buffer((display_width.into(), display_height.into()), fmt, 32)?;

            {
                let mut map = card.map_dumb_buffer(&mut db)?;
                for chunk in map.as_mut().chunks_exact_mut(4) {
                    chunk[0] = b;
                    chunk[1] = g;
                    chunk[2] = r;
                    chunk[3] = 0;
                }
            }

            let fb = card.add_framebuffer(&db, 24, 32)?;

            Ok((db, fb))
        };

        let (db_grey, fb_grey) = make_solid_fb(0x80, 0x80, 0x80)?;
        let (db_blue, fb_blue) = make_solid_fb(0x00, 0x00, 0x80)?;

        card.set_crtc(crtc, Some(fb_grey), (0, 0), &[con], Some(mode))
            .context("failed to set crtc")?;

        let five_seconds = std::time::Duration::from_secs(5);
        std::thread::sleep(five_seconds);

        card.set_crtc(crtc, Some(fb_blue), (0, 0), &[con], Some(mode))
            .context("failed to set crtc")?;

        let five_seconds = std::time::Duration::from_secs(5);
        std::thread::sleep(five_seconds);

        card.destroy_framebuffer(fb_grey)?;
        card.destroy_dumb_buffer(db_grey)?;
        card.destroy_framebuffer(fb_blue)?;
        card.destroy_dumb_buffer(db_blue)?;

        /*
        Ok(Self {
            card,
            con,
            crtc,
            mode,
            w: display_width,
            h: display_height,
            frames: [],
            front: 0,
        })
         */
        Ok(())
    }
}

fn main() -> Result<()> {
    let surface = Surface::open_default()?;

    Ok(())
}
