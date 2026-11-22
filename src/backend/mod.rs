pub mod gnome;
pub mod noop;
pub mod wayland;
pub mod wayland_overlay;
pub mod x11;

use crate::color::RenderColor;
use crate::config::BackendPreference;
use anyhow::{Context, Result};
use tracing::{info, warn};

pub trait DisplayBackend: Send {
    fn name(&self) -> &'static str;
    fn apply(&mut self, color: RenderColor) -> Result<()>;
    fn poll_events(&mut self) -> Result<()> {
        Ok(())
    }
    fn reset(&mut self) -> Result<()>;
}

/// Select and initialize the best available display backend for the current session.
pub fn create_backend(preference: BackendPreference) -> Result<Box<dyn DisplayBackend>> {
    match preference {
        BackendPreference::Wayland => {
            let b = wayland::WaylandGammaBackend::connect()
                .context("failed to initialize Wayland zwlr-gamma-control backend")?;
            Ok(Box::new(b))
        }
        BackendPreference::WaylandOverlay => {
            let b = wayland_overlay::WaylandOverlayBackend::connect()
                .context("failed to initialize Wayland zwlr-layer-shell overlay backend")?;
            Ok(Box::new(b))
        }
        BackendPreference::Gnome => {
            let b = gnome::GnomeColorBackend::connect()
                .context("failed to initialize GNOME Color D-Bus backend")?;
            Ok(Box::new(b))
        }
        BackendPreference::X11 => {
            let b = x11::X11RandrBackend::connect()
                .context("failed to initialize X11 XRandR backend")?;
            Ok(Box::new(b))
        }
        BackendPreference::Noop => Ok(Box::new(noop::NoopBackend::default())),
        BackendPreference::Auto => auto_select_backend(),
    }
}

fn auto_select_backend() -> Result<Box<dyn DisplayBackend>> {
    let has_wayland = std::env::var_os("WAYLAND_DISPLAY")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let has_x11 = std::env::var_os("DISPLAY")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    let is_wayland = has_wayland || session_type.eq_ignore_ascii_case("wayland");

    let mut errors: Vec<String> = Vec::new();

    if is_wayland {
        match wayland::WaylandGammaBackend::connect() {
            Ok(b) => {
                info!("selected display backend: {}", b.name());
                return Ok(Box::new(b));
            }
            Err(e) => {
                warn!("Wayland gamma-control unavailable: {e:#}");
                errors.push(format!("wayland-gamma: {e:#}"));
            }
        }
    }

    match gnome::GnomeColorBackend::connect() {
        Ok(b) => {
            info!("selected display backend: {}", b.name());
            return Ok(Box::new(b));
        }
        Err(e) => {
            errors.push(format!("gnome: {e:#}"));
        }
    }

    if is_wayland {
        match wayland_overlay::WaylandOverlayBackend::connect() {
            Ok(b) => {
                info!("selected display backend: {}", b.name());
                return Ok(Box::new(b));
            }
            Err(e) => {
                warn!("Wayland layer-shell overlay unavailable: {e:#}");
                errors.push(format!("wayland-overlay: {e:#}"));
            }
        }
    }

    // Only use X11 XRandR automatically on native X11 sessions (XWayland RandR is a no-op).
    if has_x11 && !is_wayland {
        match x11::X11RandrBackend::connect() {
            Ok(b) => {
                info!("selected display backend: {}", b.name());
                return Ok(Box::new(b));
            }
            Err(e) => {
                warn!("X11 XRandR backend unavailable: {e:#}");
                errors.push(format!("x11: {e:#}"));
            }
        }
    }

    anyhow::bail!("no usable display backend found ({})", errors.join("; "));
}
