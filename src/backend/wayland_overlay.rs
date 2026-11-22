use crate::backend::DisplayBackend;
use crate::color::RenderColor;
use anyhow::{Context, Result, bail};
use rustix::fs::{MemfdFlags, memfd_create};
use std::collections::HashMap;
use std::fs::File;
use std::io::ErrorKind;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::time::Duration;
use tracing::debug;
use wayland_client::backend::WaylandError;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

const RING_SLOTS: usize = 8;

/// Pre-allocated shared-memory ring of 1x1 ARGB8888 buffers.
/// Avoids creating a new `memfd` / passing an FD over `SCM_RIGHTS` every frame,
/// eliminating `EAGAIN` (os error 11) socket backpressure during 60Hz transitions.
struct ShmRing {
    file: File,
    pool: WlShmPool,
    buffers: Vec<WlBuffer>,
    in_use: [bool; RING_SLOTS],
    next_slot: usize,
    buf_w: i32,
    buf_h: i32,
    slot_bytes: usize,
}

impl ShmRing {
    fn new(
        shm: &WlShm,
        global_name: u32,
        buf_w: i32,
        buf_h: i32,
        qh: &QueueHandle<OverlayState>,
    ) -> Result<Self> {
        let slot_bytes = (buf_w as usize) * (buf_h as usize) * 4;
        let total_bytes = slot_bytes * RING_SLOTS;
        let fd = memfd_create(c"pop-nightlight-ring", MemfdFlags::CLOEXEC)
            .context("memfd_create failed for overlay ring")?;
        let file = File::from(fd);
        file.set_len(total_bytes as u64)
            .context("failed to size overlay ring memfd")?;

        let pool = shm.create_pool(file.as_fd(), total_bytes as i32, qh, ());
        let mut buffers = Vec::with_capacity(RING_SLOTS);
        for slot in 0..RING_SLOTS {
            let offset = (slot * slot_bytes) as i32;
            let buf = pool.create_buffer(
                offset,
                buf_w,
                buf_h,
                buf_w * 4,
                wl_shm::Format::Argb8888,
                qh,
                (global_name, slot),
            );
            buffers.push(buf);
        }

        Ok(Self {
            file,
            pool,
            buffers,
            in_use: [false; RING_SLOTS],
            next_slot: 0,
            buf_w,
            buf_h,
            slot_bytes,
        })
    }

    fn write_and_acquire(&mut self, pixel_bytes: [u8; 4]) -> Result<&WlBuffer> {
        let mut chosen = None;
        for offset in 0..RING_SLOTS {
            let idx = (self.next_slot + offset) % RING_SLOTS;
            if !self.in_use[idx] {
                chosen = Some(idx);
                break;
            }
        }
        let idx = chosen.unwrap_or(self.next_slot);
        self.next_slot = (idx + 1) % RING_SLOTS;

        let base_off = (idx * self.slot_bytes) as u64;
        if self.slot_bytes == 4 {
            self.file
                .write_at(&pixel_bytes, base_off)
                .context("failed to write 1x1 pixel to overlay ring")?;
        } else {
            let pixels = self.slot_bytes / 4;
            let mut full = Vec::with_capacity(self.slot_bytes);
            for _ in 0..pixels {
                full.extend_from_slice(&pixel_bytes);
            }
            self.file
                .write_at(&full, base_off)
                .context("failed to write full-screen overlay ring")?;
        }

        self.in_use[idx] = true;
        Ok(&self.buffers[idx])
    }

    fn destroy(self) {
        for b in self.buffers {
            b.destroy();
        }
        self.pool.destroy();
    }
}

struct OverlayOutput {
    output: WlOutput,
    surface: WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    viewport: Option<WpViewport>,
    ring: Option<ShmRing>,
    width: i32,
    height: i32,
    configured: bool,
    last_committed: Option<[u8; 4]>,
}

impl OverlayOutput {
    fn destroy(self) {
        if let Some(ring) = self.ring {
            ring.destroy();
        }
        if let Some(vp) = self.viewport {
            vp.destroy();
        }
        self.layer_surface.destroy();
        self.surface.destroy();
        self.output.release();
    }
}

struct OverlayState {
    compositor: WlCompositor,
    shm: WlShm,
    layer_shell: ZwlrLayerShellV1,
    viewporter: Option<WpViewporter>,
    outputs: HashMap<u32, OverlayOutput>,
    current_argb: [u8; 4],
}

impl OverlayState {
    fn create_output_overlay(
        &mut self,
        global_name: u32,
        output: WlOutput,
        qh: &QueueHandle<Self>,
    ) {
        let surface = self.compositor.create_surface(qh, ());
        // Empty input region makes the overlay 100% click-through.
        let empty_region = self.compositor.create_region(qh, ());
        surface.set_input_region(Some(&empty_region));
        empty_region.destroy();

        let layer_surface = self.layer_shell.get_layer_surface(
            &surface,
            Some(&output),
            Layer::Overlay,
            "pop-nightlight-overlay".to_string(),
            qh,
            global_name,
        );
        layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
        layer_surface.set_size(0, 0);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);

        let viewport = self
            .viewporter
            .as_ref()
            .map(|vp| vp.get_viewport(&surface, qh, ()));

        let ring = if viewport.is_some() {
            ShmRing::new(&self.shm, global_name, 1, 1, qh).ok()
        } else {
            None
        };

        // Initial bufferless commit to receive the configure event.
        surface.commit();

        self.outputs.insert(
            global_name,
            OverlayOutput {
                output,
                surface,
                layer_surface,
                viewport,
                ring,
                width: 0,
                height: 0,
                configured: false,
                last_committed: None,
            },
        );
    }

    fn commit_output(
        &mut self,
        global_name: u32,
        force: bool,
        qh: &QueueHandle<Self>,
    ) -> Result<()> {
        let current_argb = self.current_argb;
        let shm = self.shm.clone();

        let Some(entry) = self.outputs.get_mut(&global_name) else {
            return Ok(());
        };
        if !entry.configured {
            return Ok(());
        }
        if !force && entry.last_committed == Some(current_argb) {
            return Ok(());
        }

        let [a, r, g, b] = current_argb;
        let pixel: u32 = ((a as u32) << 24) | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
        let pixel_bytes = pixel.to_ne_bytes();

        if let Some(vp) = &entry.viewport {
            if entry.width > 0 && entry.height > 0 {
                vp.set_destination(entry.width, entry.height);
            }
            if entry.ring.is_none() {
                entry.ring = Some(ShmRing::new(&shm, global_name, 1, 1, qh)?);
            }
        } else {
            let w = entry.width.max(1);
            let h = entry.height.max(1);
            let needs_resize = entry
                .ring
                .as_ref()
                .map(|r| r.buf_w != w || r.buf_h != h)
                .unwrap_or(true);
            if needs_resize {
                if let Some(old) = entry.ring.take() {
                    old.destroy();
                }
                entry.ring = Some(ShmRing::new(&shm, global_name, w, h, qh)?);
            }
        }

        let Some(ring) = entry.ring.as_mut() else {
            return Ok(());
        };
        let buf_w = ring.buf_w;
        let buf_h = ring.buf_h;
        let buffer = ring.write_and_acquire(pixel_bytes)?;

        entry.surface.attach(Some(buffer), 0, 0);
        entry.surface.damage_buffer(0, 0, buf_w, buf_h);
        entry.surface.commit();
        entry.last_committed = Some(current_argb);
        Ok(())
    }

    fn commit_all(&mut self, force: bool, qh: &QueueHandle<Self>) -> Result<()> {
        let keys: Vec<u32> = self.outputs.keys().copied().collect();
        for k in keys {
            self.commit_output(k, force, qh)?;
        }
        Ok(())
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for OverlayState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == "wl_output" {
                    let output: WlOutput = registry.bind(name, version.min(4), qh, name);
                    state.create_output_overlay(name, output, qh);
                    debug!("Wayland overlay output hotplugged: global {name}");
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(entry) = state.outputs.remove(&name) {
                    entry.destroy();
                    debug!("Wayland overlay output removed: global {name}");
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlCompositor, ()> for OverlayState {
    fn event(
        _: &mut Self,
        _: &WlCompositor,
        _: <WlCompositor as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlRegion, ()> for OverlayState {
    fn event(
        _: &mut Self,
        _: &WlRegion,
        _: <WlRegion as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSurface, ()> for OverlayState {
    fn event(
        _: &mut Self,
        _: &WlSurface,
        _: <WlSurface as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShm, ()> for OverlayState {
    fn event(
        _: &mut Self,
        _: &WlShm,
        _: <WlShm as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlShmPool, ()> for OverlayState {
    fn event(
        _: &mut Self,
        _: &WlShmPool,
        _: <WlShmPool as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlBuffer, (u32, usize)> for OverlayState {
    fn event(
        state: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        &(global_name, slot): &(u32, usize),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_buffer::Event::Release)
            && let Some(entry) = state.outputs.get_mut(&global_name)
            && let Some(ring) = entry.ring.as_mut()
            && slot < RING_SLOTS
        {
            ring.in_use[slot] = false;
        }
    }
}

impl Dispatch<WlOutput, u32> for OverlayState {
    fn event(
        _: &mut Self,
        _: &WlOutput,
        _: <WlOutput as wayland_client::Proxy>::Event,
        _: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for OverlayState {
    fn event(
        _: &mut Self,
        _: &ZwlrLayerShellV1,
        _: <ZwlrLayerShellV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewporter, ()> for OverlayState {
    fn event(
        _: &mut Self,
        _: &WpViewporter,
        _: <WpViewporter as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewport, ()> for OverlayState {
    fn event(
        _: &mut Self,
        _: &WpViewport,
        _: <WpViewport as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, u32> for OverlayState {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        &global_name: &u32,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer_surface.ack_configure(serial);
                if let Some(entry) = state.outputs.get_mut(&global_name) {
                    entry.width = width as i32;
                    entry.height = height as i32;
                    entry.configured = true;
                    debug!("Wayland overlay configured for output {global_name}: {width}x{height}");
                }
                let _ = state.commit_output(global_name, true, qh);
            }
            zwlr_layer_surface_v1::Event::Closed => {
                if let Some(entry) = state.outputs.remove(&global_name) {
                    entry.destroy();
                    debug!("Wayland overlay closed for output {global_name}");
                }
            }
            _ => {}
        }
    }
}

fn flush_resilient(
    conn: &Connection,
    event_queue: &mut EventQueue<OverlayState>,
    state: &mut OverlayState,
) -> Result<()> {
    for _ in 0..30 {
        match conn.flush() {
            Ok(()) => return Ok(()),
            Err(WaylandError::Io(ref io_err)) if io_err.kind() == ErrorKind::WouldBlock => {
                if let Some(guard) = event_queue.prepare_read() {
                    let _ = guard.read();
                }
                let _ = event_queue.dispatch_pending(state);
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => return Err(e).context("Wayland overlay flush failed"),
        }
    }
    conn.flush().context("Wayland overlay flush timed out")
}

pub struct WaylandOverlayBackend {
    conn: Connection,
    event_queue: EventQueue<OverlayState>,
    state: OverlayState,
}

impl WaylandOverlayBackend {
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().context("could not connect to Wayland display")?;
        let (globals, mut event_queue) =
            registry_queue_init::<OverlayState>(&conn).context("Wayland registry init failed")?;
        let qh = event_queue.handle();

        let compositor: WlCompositor = globals
            .bind(&qh, 4..=6, ())
            .or_else(|_| globals.bind(&qh, 1..=4, ()))
            .context("compositor does not advertise wl_compositor")?;
        let shm: WlShm = globals
            .bind(&qh, 1..=1, ())
            .context("compositor does not advertise wl_shm")?;
        let layer_shell: ZwlrLayerShellV1 = globals
            .bind(&qh, 1..=4, ())
            .context("compositor does not advertise zwlr_layer_shell_v1")?;
        let viewporter: Option<WpViewporter> = globals.bind(&qh, 1..=1, ()).ok();

        let mut state = OverlayState {
            compositor,
            shm,
            layer_shell,
            viewporter,
            outputs: HashMap::new(),
            current_argb: [0, 0, 0, 0],
        };

        for global in globals.contents().clone_list() {
            if global.interface == "wl_output" {
                let output: WlOutput =
                    globals
                        .registry()
                        .bind(global.name, global.version.min(4), &qh, global.name);
                state.create_output_overlay(global.name, output, &qh);
            }
        }

        if state.outputs.is_empty() {
            bail!("no Wayland wl_output globals found");
        }

        event_queue
            .roundtrip(&mut state)
            .context("Wayland roundtrip failed during layer-shell overlay setup")?;

        Ok(Self {
            conn,
            event_queue,
            state,
        })
    }
}

impl DisplayBackend for WaylandOverlayBackend {
    fn name(&self) -> &'static str {
        "wayland-overlay (zwlr_layer_shell_v1)"
    }

    fn apply(&mut self, color: RenderColor) -> Result<()> {
        // Drain any pending Release/Configure events before writing next frame.
        if let Some(guard) = self.event_queue.prepare_read() {
            let _ = guard.read();
        }
        self.event_queue
            .dispatch_pending(&mut self.state)
            .context("Wayland overlay dispatch_pending failed")?;

        self.state.current_argb = color.overlay_argb();
        let qh = self.event_queue.handle();
        self.state.commit_all(false, &qh)?;
        flush_resilient(&self.conn, &mut self.event_queue, &mut self.state)?;
        Ok(())
    }

    fn poll_events(&mut self) -> Result<()> {
        let _ = flush_resilient(&self.conn, &mut self.event_queue, &mut self.state);
        if let Some(guard) = self.event_queue.prepare_read() {
            let _ = guard.read();
        }
        self.event_queue
            .dispatch_pending(&mut self.state)
            .context("Wayland overlay dispatch_pending failed")?;
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        let _ = self.apply(RenderColor::NEUTRAL);
        for (_, entry) in self.state.outputs.drain() {
            entry.destroy();
        }
        let _ = self.event_queue.roundtrip(&mut self.state);
        Ok(())
    }
}
