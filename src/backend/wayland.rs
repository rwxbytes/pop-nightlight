use crate::backend::DisplayBackend;
use crate::color::{RenderColor, build_gamma_lut};
use anyhow::{Context, Result, bail};
use rustix::fs::{MemfdFlags, memfd_create};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsFd;
use tracing::debug;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::gamma_control::v1::client::{
    zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1,
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};

struct OutputEntry {
    output: WlOutput,
    control: Option<ZwlrGammaControlV1>,
    gamma_size: usize,
    failed: bool,
}

struct WaylandState {
    manager: Option<ZwlrGammaControlManagerV1>,
    outputs: HashMap<u32, OutputEntry>,
}

impl WaylandState {
    fn attach_control(&mut self, global_name: u32, qh: &QueueHandle<Self>) {
        let Some(manager) = &self.manager else {
            return;
        };
        if let Some(entry) = self.outputs.get_mut(&global_name)
            && entry.control.is_none()
        {
            let ctrl = manager.get_gamma_control(&entry.output, qh, global_name);
            entry.control = Some(ctrl);
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for WaylandState {
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
                    state.outputs.insert(
                        name,
                        OutputEntry {
                            output,
                            control: None,
                            gamma_size: 0,
                            failed: false,
                        },
                    );
                    state.attach_control(name, qh);
                    debug!("Wayland output hotplugged: global {name}");
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(entry) = state.outputs.remove(&name) {
                    if let Some(ctrl) = entry.control {
                        ctrl.destroy();
                    }
                    entry.output.release();
                    debug!("Wayland output removed: global {name}");
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for WaylandState {
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

impl Dispatch<ZwlrGammaControlManagerV1, ()> for WaylandState {
    fn event(
        _: &mut Self,
        _: &ZwlrGammaControlManagerV1,
        _: <ZwlrGammaControlManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrGammaControlV1, u32> for WaylandState {
    fn event(
        state: &mut Self,
        _: &ZwlrGammaControlV1,
        event: zwlr_gamma_control_v1::Event,
        global_name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(entry) = state.outputs.get_mut(global_name) else {
            return;
        };
        match event {
            zwlr_gamma_control_v1::Event::GammaSize { size } => {
                entry.gamma_size = size as usize;
                debug!("Wayland output {global_name} gamma size = {size}");
            }
            zwlr_gamma_control_v1::Event::Failed => {
                entry.failed = true;
                entry.control = None;
                debug!("Wayland output {global_name} gamma control failed");
            }
            _ => {}
        }
    }
}

pub struct WaylandGammaBackend {
    _conn: Connection,
    event_queue: EventQueue<WaylandState>,
    state: WaylandState,
}

impl WaylandGammaBackend {
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().context("could not connect to Wayland display")?;
        let (globals, mut event_queue) =
            registry_queue_init::<WaylandState>(&conn).context("Wayland registry init failed")?;
        let qh = event_queue.handle();

        let manager: ZwlrGammaControlManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .context("compositor does not advertise zwlr_gamma_control_manager_v1")?;

        let mut outputs = HashMap::new();
        for global in globals.contents().clone_list() {
            if global.interface == "wl_output" {
                let output: WlOutput =
                    globals
                        .registry()
                        .bind(global.name, global.version.min(4), &qh, global.name);
                outputs.insert(
                    global.name,
                    OutputEntry {
                        output,
                        control: None,
                        gamma_size: 0,
                        failed: false,
                    },
                );
            }
        }

        let mut state = WaylandState {
            manager: Some(manager),
            outputs,
        };

        let keys: Vec<u32> = state.outputs.keys().copied().collect();
        for k in keys {
            state.attach_control(k, &qh);
        }

        event_queue
            .roundtrip(&mut state)
            .context("Wayland roundtrip failed during gamma control setup")?;

        if state.outputs.is_empty() {
            bail!("no Wayland wl_output globals found");
        }

        if state.outputs.values().all(|o| o.failed) {
            bail!(
                "compositor rejected zwlr_gamma_control_v1 for all outputs \
                 (another night-light client may already hold gamma control)"
            );
        }

        Ok(Self {
            _conn: conn,
            event_queue,
            state,
        })
    }
}

impl DisplayBackend for WaylandGammaBackend {
    fn name(&self) -> &'static str {
        "wayland (zwlr_gamma_control_v1)"
    }

    fn apply(&mut self, color: RenderColor) -> Result<()> {
        let multipliers = color.gamma_multipliers();
        self.event_queue
            .dispatch_pending(&mut self.state)
            .context("Wayland dispatch_pending failed")?;

        let mut applied = 0usize;
        let mut failed_count = 0usize;

        for (&global_name, entry) in &self.state.outputs {
            if entry.failed {
                failed_count += 1;
                continue;
            }
            let Some(ctrl) = &entry.control else {
                continue;
            };
            if entry.gamma_size == 0 {
                continue;
            }

            let lut = build_gamma_lut(entry.gamma_size, multipliers);
            let mut bytes = Vec::with_capacity(lut.len() * 2);
            for v in lut {
                bytes.extend_from_slice(&v.to_le_bytes());
            }

            let fd = memfd_create(c"pop-nightlight-gamma", MemfdFlags::CLOEXEC)
                .with_context(|| format!("memfd_create failed for output {global_name}"))?;
            let mut file = File::from(fd);
            file.write_all(&bytes)
                .context("failed to write gamma LUT to memfd")?;

            ctrl.set_gamma(file.as_fd());
            applied += 1;
        }

        self.event_queue
            .roundtrip(&mut self.state)
            .context("Wayland roundtrip failed after set_gamma")?;

        if applied == 0 && failed_count > 0 {
            bail!(
                "Wayland gamma control failed on all outputs \
                 (is another Night Light app holding gamma control?)"
            );
        }

        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        let _ = self.apply(RenderColor::NEUTRAL);
        for entry in self.state.outputs.values_mut() {
            if let Some(ctrl) = entry.control.take() {
                ctrl.destroy();
            }
        }
        let _ = self.event_queue.roundtrip(&mut self.state);
        Ok(())
    }
}
