use crate::backend::DisplayBackend;
use crate::color::{RenderColor, build_gamma_lut};
use anyhow::{Context, Result, bail};
use x11rb::connection::Connection;
use x11rb::protocol::randr;
use x11rb::rust_connection::RustConnection;

pub struct X11RandrBackend {
    conn: RustConnection,
    crtcs: Vec<(u32, usize)>,
}

impl X11RandrBackend {
    pub fn connect() -> Result<Self> {
        let (conn, screen_num) =
            RustConnection::connect(None).context("failed to connect to X11 display")?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;

        let resources = randr::get_screen_resources(&conn, root)
            .context("XRandR get_screen_resources request failed")?
            .reply()
            .context("XRandR get_screen_resources reply failed")?;

        let mut crtcs = Vec::new();
        for crtc in resources.crtcs {
            let size_reply = randr::get_crtc_gamma_size(&conn, crtc)
                .context("XRandR get_crtc_gamma_size request failed")?
                .reply()
                .context("XRandR get_crtc_gamma_size reply failed")?;
            if size_reply.size > 0 {
                crtcs.push((crtc, size_reply.size as usize));
            }
        }

        if crtcs.is_empty() {
            bail!("no XRandR CRTCs with non-zero gamma size found");
        }

        Ok(Self { conn, crtcs })
    }
}

impl DisplayBackend for X11RandrBackend {
    fn name(&self) -> &'static str {
        "x11 (XRandR)"
    }

    fn apply(&mut self, color: RenderColor) -> Result<()> {
        let multipliers = color.gamma_multipliers();
        for &(crtc, size) in &self.crtcs {
            let lut = build_gamma_lut(size, multipliers);
            let r = &lut[..size];
            let g = &lut[size..2 * size];
            let b = &lut[2 * size..];
            randr::set_crtc_gamma(&self.conn, crtc, r, g, b)
                .context("XRandR set_crtc_gamma failed")?;
        }
        self.conn.flush().context("X11 connection flush failed")?;
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.apply(RenderColor::NEUTRAL)
    }
}
