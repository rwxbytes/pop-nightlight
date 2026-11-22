use crate::color::{
    MAX_BRIGHTNESS, MAX_INTENSITY, MAX_TEMP_K, MIN_BRIGHTNESS, MIN_INTENSITY, MIN_TEMP_K,
};
use crate::config::ScheduleMode;
use crate::ipc::{IpcRequest, StatusSnapshot};
use anyhow::{Context, Result};
use fontdue::{Font, FontSettings};
use rustix::fs::{MemfdFlags, memfd_create};
use std::collections::HashMap;
use std::fs::File;
use std::io::ErrorKind;
use std::os::fd::AsFd;
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};
use wayland_client::backend::WaylandError;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::{self, WlBuffer};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_pointer::{self, WlPointer};
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

const CARD_W: i32 = 356;
const CARD_H: i32 = 534;
const SHADOW_PAD: i32 = 14;
const CARD_MARGIN_TOP: i32 = 6;
const CARD_MARGIN_RIGHT: i32 = 10;

// Card-local slider track X bounds (matching COSMIC Sound & Battery applets)
const SLIDER_X0: f64 = 56.0;
const SLIDER_X1: f64 = 286.0;

const BTN_LEFT: u32 = 0x110;
const KEY_ESC: u32 = 1;
const RING_SLOTS: usize = 2;

// COSMIC Dark palette sampled from native COSMIC applets
const COL_CARD_BG: [u8; 3] = [27, 27, 27];
const COL_CARD_BORDER: [u8; 3] = [58, 58, 58];
const COL_DIVIDER: [u8; 3] = [47, 47, 47];
const COL_ROW_HOVER: [u8; 3] = [42, 42, 45];
const COL_TEXT_PRIMARY: [u8; 3] = [238, 238, 238];
const COL_TEXT_MUTED: [u8; 3] = [156, 156, 156];
const COL_ACCENT: [u8; 3] = [99, 208, 223]; // Pop!_OS COSMIC cyan-teal (#63d0df)
const COL_TRACK_BG: [u8; 3] = [88, 88, 92];
const COL_SEG_BG: [u8; 3] = [38, 38, 40];
const COL_SEG_ACTIVE: [u8; 3] = [52, 54, 58];
const COL_SEG_BORDER: [u8; 3] = [62, 62, 66];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SliderKind {
    Temperature,
    Intensity,
    Brightness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x as f64
            && px <= (self.x + self.w) as f64
            && py >= self.y as f64
            && py <= (self.y + self.h) as f64
    }
}

// Card-local hit rectangles
const HEADER_ROW_RECT: Rect = Rect::new(8, 8, CARD_W - 16, 46);
const TOGGLE_RECT: Rect = Rect::new(CARD_W - 64, 19, 44, 24);

const TEMP_SLIDER_Y: i32 = 86;
const INTENSITY_SLIDER_Y: i32 = 132;
const BRIGHTNESS_SLIDER_Y: i32 = 178;

const TEMP_HIT: Rect = Rect::new(46, TEMP_SLIDER_Y - 18, 250, 36);
const INTENSITY_HIT: Rect = Rect::new(46, INTENSITY_SLIDER_Y - 18, 250, 36);
const BRIGHTNESS_HIT: Rect = Rect::new(46, BRIGHTNESS_SLIDER_Y - 18, 250, 36);

struct PresetDef {
    kelvin: u32,
    title: &'static str,
    subtitle: &'static str,
    rect: Rect,
}

const PRESETS: [PresetDef; 4] = [
    PresetDef {
        kelvin: 2400,
        title: "Warmest",
        subtitle: "2400K — Deep candlelight warmth",
        rect: Rect::new(8, 214, CARD_W - 16, 42),
    },
    PresetDef {
        kelvin: 3200,
        title: "Night",
        subtitle: "3200K — Balanced night reading",
        rect: Rect::new(8, 258, CARD_W - 16, 42),
    },
    PresetDef {
        kelvin: 4200,
        title: "Evening",
        subtitle: "4200K — Gentle evening warmth",
        rect: Rect::new(8, 302, CARD_W - 16, 42),
    },
    PresetDef {
        kelvin: 6500,
        title: "Daylight",
        subtitle: "6500K — Neutral daylight color",
        rect: Rect::new(8, 346, CARD_W - 16, 42),
    },
];

const SEG_OUTER: Rect = Rect::new(18, 434, CARD_W - 36, 34);
const MANUAL_RECT: Rect = Rect::new(18, 434, (CARD_W - 36) / 2, 34);
const SOLAR_RECT: Rect = Rect::new(18 + (CARD_W - 36) / 2, 434, (CARD_W - 36) / 2, 34);

const QUIT_RECT: Rect = Rect::new(8, 488, CARD_W - 16, 38);

/// Handle to a running floating COSMIC slider popover thread.
pub struct PopoverHandle {
    close_flag: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl PopoverHandle {
    pub fn spawn(
        status_rx: watch::Receiver<StatusSnapshot>,
        cmd_tx: mpsc::UnboundedSender<IpcRequest>,
    ) -> Self {
        let close_flag = Arc::new(AtomicBool::new(false));
        let flag = close_flag.clone();
        let join = std::thread::Builder::new()
            .name("pop-nightlight-popover".to_string())
            .spawn(move || {
                if let Err(e) = run_popover_loop(status_rx, cmd_tx, flag.clone()) {
                    warn!("slider popover exited with error: {e:#}");
                }
                flag.store(true, Ordering::SeqCst);
            })
            .ok();
        Self { close_flag, join }
    }

    pub fn is_open(&self) -> bool {
        !self.close_flag.load(Ordering::SeqCst)
    }

    pub fn close(&mut self) {
        self.close_flag.store(true, Ordering::SeqCst);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for PopoverHandle {
    fn drop(&mut self) {
        self.close();
    }
}

fn slider_fraction_from_x(card_x: f64) -> f64 {
    ((card_x - SLIDER_X0) / (SLIDER_X1 - SLIDER_X0)).clamp(0.0, 1.0)
}

fn temp_from_frac(f: f64) -> u32 {
    let raw = MIN_TEMP_K as f64 + f.clamp(0.0, 1.0) * (MAX_TEMP_K - MIN_TEMP_K) as f64;
    ((raw / 50.0).round() as u32 * 50).clamp(MIN_TEMP_K, MAX_TEMP_K)
}

fn frac_from_temp(k: u32) -> f64 {
    ((k.clamp(MIN_TEMP_K, MAX_TEMP_K) - MIN_TEMP_K) as f64) / ((MAX_TEMP_K - MIN_TEMP_K) as f64)
}

fn intensity_from_frac(f: f64) -> f64 {
    let raw = MIN_INTENSITY + f.clamp(0.0, 1.0) * (MAX_INTENSITY - MIN_INTENSITY);
    ((raw * 100.0).round() / 100.0).clamp(MIN_INTENSITY, MAX_INTENSITY)
}

fn frac_from_intensity(v: f64) -> f64 {
    ((v.clamp(MIN_INTENSITY, MAX_INTENSITY) - MIN_INTENSITY) / (MAX_INTENSITY - MIN_INTENSITY))
        .clamp(0.0, 1.0)
}

fn brightness_from_frac(f: f64) -> f64 {
    let raw = MIN_BRIGHTNESS + f.clamp(0.0, 1.0) * (MAX_BRIGHTNESS - MIN_BRIGHTNESS);
    ((raw * 100.0).round() / 100.0).clamp(MIN_BRIGHTNESS, MAX_BRIGHTNESS)
}

fn frac_from_brightness(v: f64) -> f64 {
    ((v.clamp(MIN_BRIGHTNESS, MAX_BRIGHTNESS) - MIN_BRIGHTNESS) / (MAX_BRIGHTNESS - MIN_BRIGHTNESS))
        .clamp(0.0, 1.0)
}

struct PopoverState {
    status: StatusSnapshot,
    cmd_tx: mpsc::UnboundedSender<IpcRequest>,
    should_close: Arc<AtomicBool>,
    surf_w: i32,
    surf_h: i32,
    configured: bool,
    needs_redraw: bool,
    pointer_x: f64,
    pointer_y: f64,
    pointer_inside: bool,
    dragging: Option<SliderKind>,
    in_use: [bool; RING_SLOTS],
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
}

impl PopoverState {
    fn card_origin(&self) -> (i32, i32) {
        let cx = (self.surf_w - CARD_W - CARD_MARGIN_RIGHT).max(SHADOW_PAD);
        let cy = CARD_MARGIN_TOP;
        (cx, cy)
    }

    fn to_card_coords(&self, sx: f64, sy: f64) -> (f64, f64) {
        let (cx, cy) = self.card_origin();
        (sx - cx as f64, sy - cy as f64)
    }

    fn apply_slider_at(&mut self, slider: SliderKind, card_x: f64) {
        let f = slider_fraction_from_x(card_x);
        match slider {
            SliderKind::Temperature => {
                let k = temp_from_frac(f);
                if k != self.status.temperature || !self.status.enabled {
                    self.status.temperature = k;
                    self.status.enabled = true;
                    self.status.active = true;
                    self.needs_redraw = true;
                    let _ = self.cmd_tx.send(IpcRequest::SetLive {
                        temperature: Some(k),
                        intensity: None,
                        brightness: None,
                        enable: Some(true),
                    });
                }
            }
            SliderKind::Intensity => {
                let v = intensity_from_frac(f);
                if (v - self.status.intensity).abs() > 1e-4 || !self.status.enabled {
                    self.status.intensity = v;
                    self.status.enabled = true;
                    self.status.active = true;
                    self.needs_redraw = true;
                    let _ = self.cmd_tx.send(IpcRequest::SetLive {
                        temperature: None,
                        intensity: Some(v),
                        brightness: None,
                        enable: Some(true),
                    });
                }
            }
            SliderKind::Brightness => {
                let v = brightness_from_frac(f);
                if (v - self.status.brightness).abs() > 1e-4 || !self.status.enabled {
                    self.status.brightness = v;
                    self.status.enabled = true;
                    self.status.active = true;
                    self.needs_redraw = true;
                    let _ = self.cmd_tx.send(IpcRequest::SetLive {
                        temperature: None,
                        intensity: None,
                        brightness: Some(v),
                        enable: Some(true),
                    });
                }
            }
        }
    }

    fn handle_click(&mut self, sx: f64, sy: f64) {
        let (x, y) = self.to_card_coords(sx, sy);

        // Clicking anywhere outside the card dismisses the popover (native COSMIC behavior).
        if !Rect::new(0, 0, CARD_W, CARD_H).contains(x, y) {
            self.should_close.store(true, Ordering::SeqCst);
            return;
        }

        if HEADER_ROW_RECT.contains(x, y) || TOGGLE_RECT.contains(x, y) {
            self.status.enabled = !self.status.enabled;
            self.status.active = self.status.enabled;
            self.needs_redraw = true;
            let _ = self.cmd_tx.send(IpcRequest::Toggle);
            return;
        }
        if TEMP_HIT.contains(x, y) {
            self.dragging = Some(SliderKind::Temperature);
            self.apply_slider_at(SliderKind::Temperature, x);
            return;
        }
        if INTENSITY_HIT.contains(x, y) {
            self.dragging = Some(SliderKind::Intensity);
            self.apply_slider_at(SliderKind::Intensity, x);
            return;
        }
        if BRIGHTNESS_HIT.contains(x, y) {
            self.dragging = Some(SliderKind::Brightness);
            self.apply_slider_at(SliderKind::Brightness, x);
            return;
        }
        for preset in &PRESETS {
            if preset.rect.contains(x, y) {
                self.status.temperature = preset.kelvin;
                self.status.enabled = true;
                self.status.active = true;
                self.needs_redraw = true;
                let _ = self.cmd_tx.send(IpcRequest::Set {
                    temperature: Some(preset.kelvin),
                    intensity: None,
                    brightness: None,
                    enable: true,
                });
                return;
            }
        }
        if MANUAL_RECT.contains(x, y) {
            self.status.schedule = ScheduleMode::Manual;
            self.needs_redraw = true;
            let _ = self.cmd_tx.send(IpcRequest::SetSchedule {
                schedule: ScheduleMode::Manual,
            });
            return;
        }
        if SOLAR_RECT.contains(x, y) {
            self.status.schedule = ScheduleMode::SunsetToSunrise;
            self.needs_redraw = true;
            let _ = self.cmd_tx.send(IpcRequest::SetSchedule {
                schedule: ScheduleMode::SunsetToSunrise,
            });
            return;
        }
        if QUIT_RECT.contains(x, y) {
            self.should_close.store(true, Ordering::SeqCst);
            let _ = self.cmd_tx.send(IpcRequest::Quit);
        }
    }

    fn handle_scroll(&mut self, delta: f64) {
        let (x, y) = self.to_card_coords(self.pointer_x, self.pointer_y);
        let dir = if delta < 0.0 { 1.0 } else { -1.0 };
        if TEMP_HIT.contains(x, y) {
            let next = ((self.status.temperature as i32) + (dir as i32) * 100)
                .clamp(MIN_TEMP_K as i32, MAX_TEMP_K as i32) as u32;
            self.status.temperature = next;
            self.status.enabled = true;
            self.needs_redraw = true;
            let _ = self.cmd_tx.send(IpcRequest::Set {
                temperature: Some(next),
                intensity: None,
                brightness: None,
                enable: true,
            });
        } else if INTENSITY_HIT.contains(x, y) {
            let next = (self.status.intensity + dir * 0.05).clamp(MIN_INTENSITY, MAX_INTENSITY);
            self.status.intensity = next;
            self.status.enabled = true;
            self.needs_redraw = true;
            let _ = self.cmd_tx.send(IpcRequest::Set {
                temperature: None,
                intensity: Some(next),
                brightness: None,
                enable: true,
            });
        } else if BRIGHTNESS_HIT.contains(x, y) {
            let next = (self.status.brightness + dir * 0.05).clamp(MIN_BRIGHTNESS, MAX_BRIGHTNESS);
            self.status.brightness = next;
            self.status.enabled = true;
            self.needs_redraw = true;
            let _ = self.cmd_tx.send(IpcRequest::Set {
                temperature: None,
                intensity: None,
                brightness: Some(next),
                enable: true,
            });
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for PopoverState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCompositor, ()> for PopoverState {
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

impl Dispatch<WlSurface, ()> for PopoverState {
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

impl Dispatch<WlShm, ()> for PopoverState {
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

impl Dispatch<WlShmPool, ()> for PopoverState {
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

impl Dispatch<WlBuffer, usize> for PopoverState {
    fn event(
        state: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        &slot: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, wl_buffer::Event::Release) && slot < RING_SLOTS {
            state.in_use[slot] = false;
        }
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for PopoverState {
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

impl Dispatch<ZwlrLayerSurfaceV1, ()> for PopoverState {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer_surface.ack_configure(serial);
                if width > 0 && height > 0 {
                    state.surf_w = width as i32;
                    state.surf_h = height as i32;
                }
                state.configured = true;
                state.needs_redraw = true;
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.should_close.store(true, Ordering::SeqCst);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for PopoverState {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for PopoverState {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_inside = true;
                state.pointer_x = surface_x;
                state.pointer_y = surface_y;
                state.needs_redraw = true;
            }
            wl_pointer::Event::Leave { .. } => {
                state.pointer_inside = false;
                if state.dragging.take().is_some() {
                    let _ = state.cmd_tx.send(IpcRequest::Persist);
                }
                state.needs_redraw = true;
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pointer_x = surface_x;
                state.pointer_y = surface_y;
                if let Some(slider) = state.dragging {
                    let (cx, _) = state.to_card_coords(surface_x, surface_y);
                    state.apply_slider_at(slider, cx);
                } else {
                    state.needs_redraw = true;
                }
            }
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(btn_state),
                ..
            } if button == BTN_LEFT => match btn_state {
                wl_pointer::ButtonState::Pressed => {
                    state.handle_click(state.pointer_x, state.pointer_y);
                }
                wl_pointer::ButtonState::Released if state.dragging.take().is_some() => {
                    let _ = state.cmd_tx.send(IpcRequest::Persist);
                }
                _ => {}
            },
            wl_pointer::Event::Axis {
                axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
                value,
                ..
            } if value.abs() > 0.1 => {
                state.handle_scroll(value);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for PopoverState {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Key {
            key,
            state: WEnum::Value(wl_keyboard::KeyState::Pressed),
            ..
        } = event
            && key == KEY_ESC
        {
            state.should_close.store(true, Ordering::SeqCst);
        }
    }
}

fn flush_popover(
    conn: &Connection,
    eq: &mut EventQueue<PopoverState>,
    state: &mut PopoverState,
) -> Result<()> {
    for _ in 0..20 {
        match conn.flush() {
            Ok(()) => return Ok(()),
            Err(WaylandError::Io(ref e)) if e.kind() == ErrorKind::WouldBlock => {
                if let Some(guard) = eq.prepare_read() {
                    let _ = guard.read();
                }
                let _ = eq.dispatch_pending(state);
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => return Err(e).context("popover Wayland flush failed"),
        }
    }
    Ok(())
}

struct ShmSurfaceBuffers {
    file: File,
    pool: WlShmPool,
    buffers: [WlBuffer; RING_SLOTS],
    w: i32,
    h: i32,
    slot_bytes: usize,
}

impl ShmSurfaceBuffers {
    fn new(shm: &WlShm, w: i32, h: i32, qh: &QueueHandle<PopoverState>) -> Result<Self> {
        let slot_bytes = (w as usize) * (h as usize) * 4;
        let total_bytes = slot_bytes * RING_SLOTS;
        let fd = memfd_create(c"pop-nightlight-popover", MemfdFlags::CLOEXEC)?;
        let file = File::from(fd);
        file.set_len(total_bytes as u64)?;
        let pool = shm.create_pool(file.as_fd(), total_bytes as i32, qh, ());
        let buffers = [
            pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, 0usize),
            pool.create_buffer(
                slot_bytes as i32,
                w,
                h,
                w * 4,
                wl_shm::Format::Argb8888,
                qh,
                1usize,
            ),
        ];
        Ok(Self {
            file,
            pool,
            buffers,
            w,
            h,
            slot_bytes,
        })
    }

    fn blit_card_region(
        &self,
        slot: usize,
        dst_x: i32,
        dst_y: i32,
        src_w: usize,
        src_h: usize,
        src: &[u8],
    ) -> Result<()> {
        let base = slot * self.slot_bytes;
        let row_bytes = src_w * 4;
        let surf_stride = (self.w as usize) * 4;
        for row in 0..src_h {
            let sy = dst_y + row as i32;
            if sy < 0 || sy >= self.h {
                continue;
            }
            let src_off = row * row_bytes;
            let dst_off = base + (sy as usize) * surf_stride + (dst_x as usize) * 4;
            self.file
                .write_at(&src[src_off..src_off + row_bytes], dst_off as u64)?;
        }
        Ok(())
    }

    fn destroy(self) {
        for b in self.buffers {
            b.destroy();
        }
        self.pool.destroy();
    }
}

fn run_popover_loop(
    mut status_rx: watch::Receiver<StatusSnapshot>,
    cmd_tx: mpsc::UnboundedSender<IpcRequest>,
    should_close: Arc<AtomicBool>,
) -> Result<()> {
    let fonts = FontAtlas::load();

    let conn = Connection::connect_to_env().context("popover could not connect to Wayland")?;
    let (globals, mut eq) = registry_queue_init::<PopoverState>(&conn)?;
    let qh = eq.handle();

    let compositor: WlCompositor = globals
        .bind(&qh, 4..=6, ())
        .or_else(|_| globals.bind(&qh, 1..=4, ()))
        .context("popover: wl_compositor missing")?;
    let shm: WlShm = globals
        .bind(&qh, 1..=1, ())
        .context("popover: wl_shm missing")?;
    let layer_shell: ZwlrLayerShellV1 = globals
        .bind(&qh, 1..=4, ())
        .context("popover: zwlr_layer_shell_v1 missing")?;
    let _seat: Option<WlSeat> = globals
        .bind(&qh, 4..=9, ())
        .or_else(|_| globals.bind(&qh, 1..=4, ()))
        .ok();

    let surface = compositor.create_surface(&qh, ());
    // Full-screen transparent backdrop below the top panel (`exclusive_zone = 0`):
    // gives true soft card drop shadow and native click-outside-to-dismiss.
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        None,
        Layer::Overlay,
        "pop-nightlight-controls".to_string(),
        &qh,
        (),
    );
    layer_surface.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
    layer_surface.set_size(0, 0);
    layer_surface.set_exclusive_zone(0);
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    surface.commit();

    let mut state = PopoverState {
        status: status_rx.borrow().clone(),
        cmd_tx,
        should_close: should_close.clone(),
        surf_w: CARD_W + SHADOW_PAD * 2,
        surf_h: CARD_H + SHADOW_PAD * 2,
        configured: false,
        needs_redraw: true,
        pointer_x: -1.0,
        pointer_y: -1.0,
        pointer_inside: false,
        dragging: None,
        in_use: [false; RING_SLOTS],
        pointer: None,
        keyboard: None,
    };

    eq.roundtrip(&mut state)?;
    debug!(
        "COSMIC popover configured (surface {}x{}, card {}x{})",
        state.surf_w, state.surf_h, CARD_W, CARD_H
    );

    let mut shm_bufs: Option<ShmSurfaceBuffers> = None;
    let region_w = (CARD_W + SHADOW_PAD * 2) as usize;
    let region_h = (CARD_H + SHADOW_PAD * 2) as usize;
    let mut card_buf = vec![0u8; region_w * region_h * 4];
    let mut next_slot = 0usize;
    let mut first_paint = true;

    while !should_close.load(Ordering::SeqCst) {
        if status_rx.has_changed().unwrap_or(false) {
            let latest = status_rx.borrow_and_update().clone();
            if state.dragging.is_none() && latest != state.status {
                state.status = latest;
                state.needs_redraw = true;
            }
        }

        if state.configured && state.needs_redraw {
            let needs_alloc = shm_bufs
                .as_ref()
                .map(|b| b.w != state.surf_w || b.h != state.surf_h)
                .unwrap_or(true);
            if needs_alloc {
                if let Some(old) = shm_bufs.take() {
                    old.destroy();
                }
                shm_bufs = Some(ShmSurfaceBuffers::new(
                    &shm,
                    state.surf_w,
                    state.surf_h,
                    &qh,
                )?);
                state.in_use = [false; RING_SLOTS];
                first_paint = true;
            }

            if let Some(bufs) = shm_bufs.as_ref() {
                let slot = if !state.in_use[next_slot] {
                    next_slot
                } else if !state.in_use[1 - next_slot] {
                    1 - next_slot
                } else {
                    next_slot
                };
                next_slot = 1 - slot;

                let (card_x, card_y) = state.card_origin();
                let dst_x = (card_x - SHADOW_PAD).clamp(0, (bufs.w - region_w as i32).max(0));
                let dst_y = (card_y - SHADOW_PAD).clamp(0, (bufs.h - region_h as i32).max(0));
                let card_in_region_x = card_x - dst_x;
                let card_in_region_y = card_y - dst_y;

                let hover = state
                    .pointer_inside
                    .then(|| state.to_card_coords(state.pointer_x, state.pointer_y));

                render_cosmic_card(
                    &mut card_buf,
                    region_w,
                    region_h,
                    card_in_region_x,
                    card_in_region_y,
                    &state.status,
                    hover,
                    state.dragging,
                    &fonts,
                );

                bufs.blit_card_region(slot, dst_x, dst_y, region_w, region_h, &card_buf)?;
                state.in_use[slot] = true;
                surface.attach(Some(&bufs.buffers[slot]), 0, 0);
                if first_paint {
                    surface.damage_buffer(0, 0, bufs.w, bufs.h);
                    first_paint = false;
                } else {
                    surface.damage_buffer(dst_x, dst_y, region_w as i32, region_h as i32);
                }
                surface.commit();
                state.needs_redraw = false;
            }
        }

        flush_popover(&conn, &mut eq, &mut state)?;
        if let Some(guard) = eq.prepare_read() {
            let _ = guard.read();
        }
        eq.dispatch_pending(&mut state)?;
        std::thread::sleep(Duration::from_millis(12));
    }

    if let Some(p) = state.pointer.take() {
        p.release();
    }
    if let Some(k) = state.keyboard.take() {
        k.release();
    }
    if let Some(bufs) = shm_bufs.take() {
        bufs.destroy();
    }
    layer_surface.destroy();
    surface.destroy();
    let _ = eq.roundtrip(&mut state);
    Ok(())
}

// ---------- TrueType/OpenType Font Loader & Cache (Fira Sans — native COSMIC font) ----------

struct FontAtlas {
    regular: Option<Font>,
    medium: Option<Font>,
}

impl FontAtlas {
    fn load() -> Self {
        let regular = load_first_font(&[
            "/usr/share/fonts/opentype/fira/FiraSans-Regular.otf",
            "/usr/share/fonts/fira/FiraSans-Regular.otf",
            "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/truetype/ubuntu/Ubuntu-R.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
        ]);
        let medium = load_first_font(&[
            "/usr/share/fonts/opentype/fira/FiraSans-Medium.otf",
            "/usr/share/fonts/fira/FiraSans-Medium.otf",
            "/usr/share/fonts/truetype/noto/NotoSans-Medium.ttf",
            "/usr/share/fonts/noto/NotoSans-Medium.ttf",
            "/usr/share/fonts/truetype/ubuntu/Ubuntu-M.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
            "/usr/share/fonts/TTF/DejaVuSans-Bold.ttf",
        ]);
        if regular.is_none() && medium.is_none() {
            warn!("no system sans-serif font found; popover labels will be hidden");
        }
        Self { regular, medium }
    }

    fn font(&self, bold: bool) -> Option<&Font> {
        if bold {
            self.medium.as_ref().or(self.regular.as_ref())
        } else {
            self.regular.as_ref().or(self.medium.as_ref())
        }
    }
}

fn load_first_font(paths: &[&str]) -> Option<Font> {
    for p in paths {
        if let Ok(bytes) = std::fs::read(p)
            && let Ok(font) = Font::from_bytes(bytes, FontSettings::default())
        {
            return Some(font);
        }
    }
    None
}

// ---------- Software Anti-Aliased ARGB8888 COSMIC Renderer ----------

struct Canvas<'a> {
    buf: &'a mut [u8],
    w: usize,
    h: usize,
    ox: i32,
    oy: i32,
    glyph_cache: HashMap<(bool, u32, char), (fontdue::Metrics, Vec<u8>)>,
}

impl<'a> Canvas<'a> {
    fn new(buf: &'a mut [u8], w: usize, h: usize, ox: i32, oy: i32) -> Self {
        buf.fill(0);
        Self {
            buf,
            w,
            h,
            ox,
            oy,
            glyph_cache: HashMap::new(),
        }
    }

    #[inline]
    fn blend_pixel(&mut self, lx: i32, ly: i32, r: u8, g: u8, b: u8, a: f64) {
        let x = lx + self.ox;
        let y = ly + self.oy;
        if x < 0 || y < 0 || (x as usize) >= self.w || (y as usize) >= self.h || a <= 0.001 {
            return;
        }
        let a = a.clamp(0.0, 1.0);
        let idx = ((y as usize) * self.w + (x as usize)) * 4;
        let dst = u32::from_ne_bytes([
            self.buf[idx],
            self.buf[idx + 1],
            self.buf[idx + 2],
            self.buf[idx + 3],
        ]);
        let dst_a = ((dst >> 24) & 0xFF) as f64 / 255.0;
        let dst_r = ((dst >> 16) & 0xFF) as f64 / 255.0;
        let dst_g = ((dst >> 8) & 0xFF) as f64 / 255.0;
        let dst_b = (dst & 0xFF) as f64 / 255.0;

        let inv = 1.0 - a;
        let out_a = (a + dst_a * inv).clamp(0.0, 1.0);
        let out_r = ((r as f64 / 255.0) * a + dst_r * inv).clamp(0.0, out_a);
        let out_g = ((g as f64 / 255.0) * a + dst_g * inv).clamp(0.0, out_a);
        let out_b = ((b as f64 / 255.0) * a + dst_b * inv).clamp(0.0, out_a);

        let packed: u32 = (((out_a * 255.0).round() as u32) << 24)
            | (((out_r * 255.0).round() as u32) << 16)
            | (((out_g * 255.0).round() as u32) << 8)
            | ((out_b * 255.0).round() as u32);
        self.buf[idx..idx + 4].copy_from_slice(&packed.to_ne_bytes());
    }

    fn draw_card_shadow(&mut self, rect: Rect, radius: f64) {
        let spread = 10;
        let x0 = rect.x - spread;
        let y0 = rect.y - spread + 3;
        let x1 = rect.x + rect.w + spread;
        let y1 = rect.y + rect.h + spread + 4;
        let cx = rect.x as f64 + rect.w as f64 * 0.5;
        let cy = rect.y as f64 + rect.h as f64 * 0.5 + 3.0;
        let hx = rect.w as f64 * 0.5 - radius;
        let hy = rect.h as f64 * 0.5 - radius;

        for y in y0..y1 {
            for x in x0..x1 {
                let dx = ((x as f64 + 0.5) - cx).abs() - hx;
                let dy = ((y as f64 + 0.5) - cy).abs() - hy;
                let dist = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() + dx.max(dy).min(0.0)
                    - radius;
                if dist > -1.0 && dist < spread as f64 {
                    let t = (1.0 - (dist.max(0.0) / spread as f64)).powi(2);
                    self.blend_pixel(x, y, 0, 0, 0, t * 0.42);
                }
            }
        }
    }

    fn fill_rounded_rect(&mut self, rect: Rect, radius: f64, color: [u8; 3], alpha: f64) {
        let x0 = rect.x;
        let y0 = rect.y;
        let x1 = rect.x + rect.w;
        let y1 = rect.y + rect.h;
        let cx = rect.x as f64 + rect.w as f64 * 0.5;
        let cy = rect.y as f64 + rect.h as f64 * 0.5;
        let hx = rect.w as f64 * 0.5 - radius;
        let hy = rect.h as f64 * 0.5 - radius;

        for y in y0..y1 {
            for x in x0..x1 {
                let dx = ((x as f64 + 0.5) - cx).abs() - hx;
                let dy = ((y as f64 + 0.5) - cy).abs() - hy;
                let dist = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() + dx.max(dy).min(0.0)
                    - radius;
                let cov = (0.5 - dist).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend_pixel(x, y, color[0], color[1], color[2], cov * alpha);
                }
            }
        }
    }

    fn stroke_rounded_rect(
        &mut self,
        rect: Rect,
        radius: f64,
        thickness: f64,
        color: [u8; 3],
        alpha: f64,
    ) {
        let x0 = rect.x - 1;
        let y0 = rect.y - 1;
        let x1 = rect.x + rect.w + 1;
        let y1 = rect.y + rect.h + 1;
        let cx = rect.x as f64 + rect.w as f64 * 0.5;
        let cy = rect.y as f64 + rect.h as f64 * 0.5;
        let hx = rect.w as f64 * 0.5 - radius;
        let hy = rect.h as f64 * 0.5 - radius;
        let half_t = thickness * 0.5;

        for y in y0..y1 {
            for x in x0..x1 {
                let dx = ((x as f64 + 0.5) - cx).abs() - hx;
                let dy = ((y as f64 + 0.5) - cy).abs() - hy;
                let dist = (dx.max(0.0).powi(2) + dy.max(0.0).powi(2)).sqrt() + dx.max(dy).min(0.0)
                    - radius;
                let band = (dist + half_t).abs() - half_t;
                let cov = (0.5 - band).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend_pixel(x, y, color[0], color[1], color[2], cov * alpha);
                }
            }
        }
    }

    fn fill_circle(&mut self, cx: f64, cy: f64, r: f64, color: [u8; 3], alpha: f64) {
        let x0 = (cx - r - 1.0).floor() as i32;
        let y0 = (cy - r - 1.0).floor() as i32;
        let x1 = (cx + r + 2.0).ceil() as i32;
        let y1 = (cy + r + 2.0).ceil() as i32;
        for y in y0..y1 {
            for x in x0..x1 {
                let d = ((x as f64 + 0.5 - cx).powi(2) + (y as f64 + 0.5 - cy).powi(2)).sqrt();
                let cov = (r + 0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend_pixel(x, y, color[0], color[1], color[2], cov * alpha);
                }
            }
        }
    }

    fn stroke_circle(&mut self, cx: f64, cy: f64, r: f64, thickness: f64, color: [u8; 3]) {
        let half_t = thickness * 0.5;
        let x0 = (cx - r - thickness - 1.0).floor() as i32;
        let y0 = (cy - r - thickness - 1.0).floor() as i32;
        let x1 = (cx + r + thickness + 2.0).ceil() as i32;
        let y1 = (cy + r + thickness + 2.0).ceil() as i32;
        for y in y0..y1 {
            for x in x0..x1 {
                let d = ((x as f64 + 0.5 - cx).powi(2) + (y as f64 + 0.5 - cy).powi(2)).sqrt();
                let cov = (0.5 - ((d - r).abs() - half_t)).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend_pixel(x, y, color[0], color[1], color[2], cov);
                }
            }
        }
    }

    fn draw_line(&mut self, x0: f64, y0: f64, x1: f64, y1: f64, thickness: f64, color: [u8; 3]) {
        let min_x = (x0.min(x1) - thickness - 1.0).floor() as i32;
        let max_x = (x0.max(x1) + thickness + 2.0).ceil() as i32;
        let min_y = (y0.min(y1) - thickness - 1.0).floor() as i32;
        let max_y = (y0.max(y1) + thickness + 2.0).ceil() as i32;
        let dx = x1 - x0;
        let dy = y1 - y0;
        let len_sq = (dx * dx + dy * dy).max(1e-6);
        let half_t = thickness * 0.5;

        for y in min_y..max_y {
            for x in min_x..max_x {
                let px = x as f64 + 0.5;
                let py = y as f64 + 0.5;
                let t = (((px - x0) * dx + (py - y0) * dy) / len_sq).clamp(0.0, 1.0);
                let proj_x = x0 + t * dx;
                let proj_y = y0 + t * dy;
                let d = ((px - proj_x).powi(2) + (py - proj_y).powi(2)).sqrt();
                let cov = (half_t + 0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend_pixel(x, y, color[0], color[1], color[2], cov);
                }
            }
        }
    }

    fn measure_text(&self, fonts: &FontAtlas, text: &str, size: f32, bold: bool) -> f32 {
        let Some(font) = fonts.font(bold) else {
            return text.chars().count() as f32 * size * 0.55;
        };
        text.chars()
            .map(|ch| font.metrics(ch, size).advance_width)
            .sum()
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text(
        &mut self,
        fonts: &FontAtlas,
        text: &str,
        x: f32,
        baseline_y: f32,
        size: f32,
        bold: bool,
        color: [u8; 3],
    ) {
        let Some(font) = fonts.font(bold) else {
            return;
        };
        let size_key = (size * 10.0).round() as u32;
        let mut cursor_x = x;

        for ch in text.chars() {
            let key = (bold, size_key, ch);
            let (metrics, bitmap) = self
                .glyph_cache
                .entry(key)
                .or_insert_with(|| font.rasterize(ch, size))
                .clone();

            let gx0 = (cursor_x + metrics.xmin as f32).round() as i32;
            let gy0 = (baseline_y - metrics.height as f32 - metrics.ymin as f32).round() as i32;

            for row in 0..metrics.height {
                for col in 0..metrics.width {
                    let cov = bitmap[row * metrics.width + col];
                    if cov > 0 {
                        self.blend_pixel(
                            gx0 + col as i32,
                            gy0 + row as i32,
                            color[0],
                            color[1],
                            color[2],
                            cov as f64 / 255.0,
                        );
                    }
                }
            }
            cursor_x += metrics.advance_width;
        }
    }
}

// ---------- Native COSMIC Monochrome Vector Icons ----------

/// Sun / warmth icon (left of Temperature slider).
fn draw_icon_sun(c: &mut Canvas<'_>, cx: f64, cy: f64, color: [u8; 3]) {
    c.stroke_circle(cx, cy, 4.2, 1.5, color);
    for i in 0..8 {
        let ang = (i as f64) * std::f64::consts::FRAC_PI_4;
        let r0 = 6.5;
        let r1 = 8.5;
        c.draw_line(
            cx + ang.cos() * r0,
            cy + ang.sin() * r0,
            cx + ang.cos() * r1,
            cy + ang.sin() * r1,
            1.4,
            color,
        );
    }
}

/// Half-filled circle contrast / intensity icon (left of Intensity slider).
fn draw_icon_intensity(c: &mut Canvas<'_>, cx: f64, cy: f64, color: [u8; 3]) {
    let r = 7.4;
    c.stroke_circle(cx, cy, r, 1.5, color);
    let x0 = cx.floor() as i32;
    let x1 = (cx + r + 1.0).ceil() as i32;
    let y0 = (cy - r - 1.0).floor() as i32;
    let y1 = (cy + r + 2.0).ceil() as i32;
    for y in y0..y1 {
        for x in x0..x1 {
            if (x as f64 + 0.5) >= cx {
                let d = ((x as f64 + 0.5 - cx).powi(2) + (y as f64 + 0.5 - cy).powi(2)).sqrt();
                let cov = (r - 0.8 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    c.blend_pixel(x, y, color[0], color[1], color[2], cov);
                }
            }
        }
    }
}

/// Monitor / display icon (matching COSMIC Battery applet brightness icon!).
fn draw_icon_display(c: &mut Canvas<'_>, cx: f64, cy: f64, color: [u8; 3]) {
    let x = cx.round() as i32;
    let y = cy.round() as i32;
    c.stroke_rounded_rect(Rect::new(x - 8, y - 7, 16, 11), 2.0, 1.5, color, 1.0);
    // Stand neck + base
    c.draw_line(cx, (y + 4) as f64, cx, (y + 7) as f64, 1.6, color);
    c.draw_line(
        (x - 4) as f64,
        (y + 7) as f64,
        (x + 4) as f64,
        (y + 7) as f64,
        1.5,
        color,
    );
}

/// Crisp COSMIC checkmark (`✓`) drawn with anti-aliased vector strokes.
fn draw_checkmark(c: &mut Canvas<'_>, cx: f64, cy: f64, color: [u8; 3]) {
    c.draw_line(cx - 4.5, cy + 0.2, cx - 1.2, cy + 3.5, 1.6, color);
    c.draw_line(cx - 1.2, cy + 3.5, cx + 4.8, cy - 3.5, 1.6, color);
}

/// COSMIC native slider row: icon | thin 4px track + solid cyan thumb | right readout.
#[allow(clippy::too_many_arguments)]
fn draw_cosmic_slider(
    c: &mut Canvas<'_>,
    fonts: &FontAtlas,
    y_center: i32,
    frac: f64,
    tick_frac: Option<f64>,
    readout: &str,
    active_hover: bool,
    enabled: bool,
) {
    let x0 = SLIDER_X0 as i32;
    let w = (SLIDER_X1 - SLIDER_X0) as i32;
    let fill_col = if enabled { COL_ACCENT } else { [130, 130, 136] };

    // Thin 4px COSMIC track
    c.fill_rounded_rect(Rect::new(x0, y_center - 2, w, 4), 2.0, COL_TRACK_BG, 1.0);
    let fill_w = ((frac.clamp(0.0, 1.0) * w as f64).round() as i32).max(0);
    if fill_w > 2 {
        c.fill_rounded_rect(Rect::new(x0, y_center - 2, fill_w, 4), 2.0, fill_col, 1.0);
    }

    // Subtle default tick mark below track (just like COSMIC Sound's 100% mark!)
    if let Some(tf) = tick_frac {
        let tx = SLIDER_X0 + tf.clamp(0.0, 1.0) * (SLIDER_X1 - SLIDER_X0);
        c.draw_line(
            tx,
            (y_center + 7) as f64,
            tx,
            (y_center + 11) as f64,
            1.2,
            [132, 132, 138],
        );
    }

    // Solid circular COSMIC thumb (cyan)
    let thumb_x = SLIDER_X0 + frac.clamp(0.0, 1.0) * (SLIDER_X1 - SLIDER_X0);
    let thumb_r = if active_hover { 9.5 } else { 8.5 };
    c.fill_circle(thumb_x, y_center as f64, thumb_r, fill_col, 1.0);

    // Right-aligned tabular readout
    let rw = c.measure_text(fonts, readout, 13.5, false);
    c.draw_text(
        fonts,
        readout,
        (CARD_W as f32 - 18.0) - rw,
        y_center as f32 + 4.5,
        13.5,
        false,
        COL_TEXT_PRIMARY,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_cosmic_card(
    buf: &mut [u8],
    buf_w: usize,
    buf_h: usize,
    ox: i32,
    oy: i32,
    status: &StatusSnapshot,
    hover: Option<(f64, f64)>,
    dragging: Option<SliderKind>,
    fonts: &FontAtlas,
) {
    let mut c = Canvas::new(buf, buf_w, buf_h, ox, oy);
    let is_hover = |r: Rect| hover.is_some_and(|(x, y)| r.contains(x, y));

    // Drop shadow + COSMIC dark rounded card + crisp 1px border
    let card = Rect::new(0, 0, CARD_W, CARD_H);
    c.draw_card_shadow(card, 16.0);
    c.fill_rounded_rect(card, 16.0, COL_CARD_BG, 0.985);
    c.stroke_rounded_rect(card, 16.0, 1.0, COL_CARD_BORDER, 1.0);

    // ---------- Section 1: Header + COSMIC Toggle Switch ----------
    if is_hover(HEADER_ROW_RECT) {
        c.fill_rounded_rect(HEADER_ROW_RECT, 10.0, COL_ROW_HOVER, 0.75);
    }
    c.draw_text(
        fonts,
        "Night Light",
        20.0,
        29.0,
        14.5,
        true,
        COL_TEXT_PRIMARY,
    );
    let sub = if status.active {
        format!("{}K active", status.temperature)
    } else if status.enabled {
        "Scheduled (daytime)".to_string()
    } else {
        "Off".to_string()
    };
    c.draw_text(fonts, &sub, 20.0, 45.0, 11.5, false, COL_TEXT_MUTED);

    // Native COSMIC pill toggle switch
    if status.enabled {
        c.fill_rounded_rect(TOGGLE_RECT, 12.0, COL_ACCENT, 1.0);
        let kx = (TOGGLE_RECT.x + TOGGLE_RECT.w - 12) as f64;
        let ky = (TOGGLE_RECT.y + 12) as f64;
        c.fill_circle(kx, ky, 8.2, [242, 252, 254], 1.0);
    } else {
        c.fill_rounded_rect(TOGGLE_RECT, 12.0, [34, 34, 36], 1.0);
        c.stroke_rounded_rect(TOGGLE_RECT, 12.0, 1.4, [130, 130, 136], 1.0);
        let kx = (TOGGLE_RECT.x + 12) as f64;
        let ky = (TOGGLE_RECT.y + 12) as f64;
        c.fill_circle(kx, ky, 7.6, [168, 168, 174], 1.0);
    }

    // Divider 1
    c.fill_rounded_rect(Rect::new(16, 60, CARD_W - 32, 1), 0.5, COL_DIVIDER, 1.0);

    // ---------- Section 2: Native COSMIC Sliders ----------
    let icon_col = [222, 222, 226];
    draw_icon_sun(&mut c, 30.0, TEMP_SLIDER_Y as f64, icon_col);
    draw_cosmic_slider(
        &mut c,
        fonts,
        TEMP_SLIDER_Y,
        frac_from_temp(status.temperature),
        Some(frac_from_temp(3200)),
        &format!("{}K", status.temperature),
        dragging == Some(SliderKind::Temperature) || is_hover(TEMP_HIT),
        status.enabled,
    );

    draw_icon_intensity(&mut c, 30.0, INTENSITY_SLIDER_Y as f64, icon_col);
    draw_cosmic_slider(
        &mut c,
        fonts,
        INTENSITY_SLIDER_Y,
        frac_from_intensity(status.intensity),
        Some(frac_from_intensity(0.70)),
        &format!("{}%", (status.intensity * 100.0).round() as u32),
        dragging == Some(SliderKind::Intensity) || is_hover(INTENSITY_HIT),
        status.enabled,
    );

    draw_icon_display(&mut c, 30.0, BRIGHTNESS_SLIDER_Y as f64, icon_col);
    draw_cosmic_slider(
        &mut c,
        fonts,
        BRIGHTNESS_SLIDER_Y,
        frac_from_brightness(status.brightness),
        None,
        &format!("{}%", (status.brightness * 100.0).round() as u32),
        dragging == Some(SliderKind::Brightness) || is_hover(BRIGHTNESS_HIT),
        status.enabled,
    );

    // Divider 2
    c.fill_rounded_rect(Rect::new(16, 206, CARD_W - 32, 1), 0.5, COL_DIVIDER, 1.0);

    // ---------- Section 3: Temperature Presets (Power Profile list style) ----------
    for preset in &PRESETS {
        let selected = status.enabled && status.temperature.abs_diff(preset.kelvin) <= 50;
        if is_hover(preset.rect) {
            c.fill_rounded_rect(preset.rect, 10.0, COL_ROW_HOVER, 0.9);
        }
        c.draw_text(
            fonts,
            preset.title,
            20.0,
            preset.rect.y as f32 + 18.0,
            13.8,
            false,
            COL_TEXT_PRIMARY,
        );
        c.draw_text(
            fonts,
            preset.subtitle,
            20.0,
            preset.rect.y as f32 + 34.0,
            11.3,
            false,
            COL_TEXT_MUTED,
        );
        if selected {
            draw_checkmark(
                &mut c,
                (CARD_W - 26) as f64,
                preset.rect.y as f64 + preset.rect.h as f64 * 0.5,
                COL_TEXT_PRIMARY,
            );
        }
    }

    // Divider 3
    c.fill_rounded_rect(Rect::new(16, 396, CARD_W - 32, 1), 0.5, COL_DIVIDER, 1.0);

    // ---------- Section 4: Schedule Segmented Pill (Window Management style) ----------
    c.draw_text(
        fonts,
        "Schedule",
        20.0,
        421.0,
        13.5,
        false,
        COL_TEXT_PRIMARY,
    );
    if let Some(next) = &status.next_transition {
        let nw = c.measure_text(fonts, next, 11.5, false);
        c.draw_text(
            fonts,
            next,
            (CARD_W as f32 - 20.0) - nw,
            421.0,
            11.5,
            false,
            COL_TEXT_MUTED,
        );
    }

    // Outer segmented container
    c.fill_rounded_rect(SEG_OUTER, 17.0, COL_SEG_BG, 1.0);

    let is_manual = status.schedule == ScheduleMode::Manual;
    let active_seg = if is_manual { MANUAL_RECT } else { SOLAR_RECT };
    c.fill_rounded_rect(active_seg, 16.0, COL_SEG_ACTIVE, 1.0);

    // Hover tint on inactive segment
    let inactive_seg = if is_manual { SOLAR_RECT } else { MANUAL_RECT };
    if is_hover(inactive_seg) {
        c.fill_rounded_rect(inactive_seg, 16.0, [46, 46, 50], 0.85);
    }

    // Hairline center separator + outer border
    let mid_x = SEG_OUTER.x + SEG_OUTER.w / 2;
    c.fill_rounded_rect(
        Rect::new(mid_x, SEG_OUTER.y + 5, 1, SEG_OUTER.h - 10),
        0.5,
        COL_SEG_BORDER,
        1.0,
    );
    c.stroke_rounded_rect(SEG_OUTER, 17.0, 1.0, COL_SEG_BORDER, 1.0);

    // Segment labels + COSMIC cyan checkmark on selected side
    for (rect, label, active) in [
        (MANUAL_RECT, "Manual", is_manual),
        (SOLAR_RECT, "Sunset to Sunrise", !is_manual),
    ] {
        let col = if active { COL_ACCENT } else { COL_TEXT_MUTED };
        let tw = c.measure_text(fonts, label, 12.8, active);
        let check_w = if active { 15.0 } else { 0.0 };
        let start_x = rect.x as f32 + (rect.w as f32 - (tw + check_w)) * 0.5;
        let baseline = rect.y as f32 + 21.5;
        if active {
            draw_checkmark(
                &mut c,
                (start_x + 4.0) as f64,
                rect.y as f64 + 17.0,
                COL_ACCENT,
            );
        }
        c.draw_text(fonts, label, start_x + check_w, baseline, 12.8, active, col);
    }

    // Divider 4
    c.fill_rounded_rect(Rect::new(16, 480, CARD_W - 32, 1), 0.5, COL_DIVIDER, 1.0);

    // ---------- Section 5: Footer Action ----------
    if is_hover(QUIT_RECT) {
        c.fill_rounded_rect(QUIT_RECT, 10.0, COL_ROW_HOVER, 0.9);
    }
    c.draw_text(
        fonts,
        "Quit Night Light",
        20.0,
        QUIT_RECT.y as f32 + 24.0,
        13.5,
        false,
        COL_TEXT_PRIMARY,
    );
    let hint = "Restores 6500K";
    let hw = c.measure_text(fonts, hint, 11.5, false);
    c.draw_text(
        fonts,
        hint,
        (CARD_W as f32 - 20.0) - hw,
        QUIT_RECT.y as f32 + 23.5,
        11.5,
        false,
        COL_TEXT_MUTED,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slider_conversions_roundtrip_cleanly() {
        assert_eq!(temp_from_frac(0.0), MIN_TEMP_K);
        assert_eq!(temp_from_frac(1.0), MAX_TEMP_K);
        assert_eq!(temp_from_frac(frac_from_temp(3200)), 3200);

        assert!((intensity_from_frac(0.0) - MIN_INTENSITY).abs() < 1e-9);
        assert!((intensity_from_frac(1.0) - MAX_INTENSITY).abs() < 1e-9);
        assert!((brightness_from_frac(0.0) - MIN_BRIGHTNESS).abs() < 1e-9);
        assert!((brightness_from_frac(1.0) - MAX_BRIGHTNESS).abs() < 1e-9);
    }

    #[test]
    fn render_cosmic_card_populates_valid_argb_buffer() {
        let w = (CARD_W + SHADOW_PAD * 2) as usize;
        let h = (CARD_H + SHADOW_PAD * 2) as usize;
        let mut buf = vec![0u8; w * h * 4];
        let snap = StatusSnapshot {
            active: true,
            enabled: true,
            temperature: 3200,
            intensity: 0.7,
            brightness: 1.0,
            schedule: ScheduleMode::Manual,
            backend: "wayland-overlay".to_string(),
            solar_is_night: None,
            next_transition: None,
        };
        let fonts = FontAtlas::load();
        render_cosmic_card(
            &mut buf,
            w,
            h,
            SHADOW_PAD,
            SHADOW_PAD,
            &snap,
            Some((100.0, 100.0)),
            None,
            &fonts,
        );
        let center_idx = ((h / 2) * w + (w / 2)) * 4;
        let px = u32::from_ne_bytes([
            buf[center_idx],
            buf[center_idx + 1],
            buf[center_idx + 2],
            buf[center_idx + 3],
        ]);
        assert!((px >> 24) > 200);
    }
}
