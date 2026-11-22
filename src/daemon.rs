use crate::backend::{DisplayBackend, create_backend};
use crate::color::{RenderColor, clamp_brightness, clamp_intensity, clamp_temperature};
use crate::config::{BackendPreference, Config, ScheduleMode};
use crate::ipc::{
    IpcRequest, IpcResponse, StatusSnapshot, bind_listener, default_socket_path, read_request,
    write_response,
};
use crate::popover::PopoverHandle;
use crate::solar::solar_status_at;
use crate::tray::NightLightTray;
use anyhow::Result;
use chrono::{Local, Utc};
use ksni::TrayMethods;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{error, info, warn};

pub struct DaemonOptions {
    pub config_path: PathBuf,
    pub socket_path: PathBuf,
    pub backend_override: Option<BackendPreference>,
    pub no_tray: bool,
    pub open_controls_on_start: bool,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            config_path: crate::config::default_config_path(),
            socket_path: default_socket_path(),
            backend_override: None,
            no_tray: false,
            open_controls_on_start: false,
        }
    }
}

struct DaemonState {
    config: Config,
    config_path: PathBuf,
    backend: Box<dyn DisplayBackend>,
    current_color: RenderColor,
    start_color: RenderColor,
    target_color: RenderColor,
    transition_start: Option<Instant>,
}

enum RequestOutcome {
    Continue,
    ToggleControls,
    Quit,
}

impl DaemonState {
    fn evaluate_active_and_target(&self) -> (bool, RenderColor, Option<bool>, Option<String>) {
        if !self.config.enabled {
            return (false, RenderColor::NEUTRAL, None, None);
        }

        let target_on = RenderColor::from_settings(
            self.config.temperature,
            self.config.intensity,
            self.config.brightness,
        );

        match self.config.schedule {
            ScheduleMode::Manual => (true, target_on, None, None),
            ScheduleMode::SunsetToSunrise => {
                let solar =
                    solar_status_at(Utc::now(), self.config.latitude, self.config.longitude);
                let next_str = solar.next_transition.map(|(ts, will_be_night)| {
                    let local_ts = ts.with_timezone(&Local);
                    let label = if will_be_night { "sunset" } else { "sunrise" };
                    format!("{label} {}", local_ts.format("%H:%M"))
                });
                let target = if solar.is_night {
                    target_on
                } else {
                    RenderColor::NEUTRAL
                };
                (solar.is_night, target, Some(solar.is_night), next_str)
            }
        }
    }

    fn snapshot(&self) -> StatusSnapshot {
        let (active, _, solar_is_night, next_transition) = self.evaluate_active_and_target();
        StatusSnapshot {
            active,
            enabled: self.config.enabled,
            temperature: self.config.temperature,
            intensity: self.config.intensity,
            brightness: self.config.brightness,
            schedule: self.config.schedule,
            backend: self.backend.name().to_string(),
            solar_is_night,
            next_transition,
        }
    }

    fn retarget(&mut self, instant: bool) {
        let (_, new_target, _, _) = self.evaluate_active_and_target();
        if new_target.approx_eq(&self.target_color) && (!instant || self.transition_start.is_none())
        {
            return;
        }
        self.start_color = self.current_color;
        self.target_color = new_target;
        if instant || self.config.transition_ms == 0 {
            self.current_color = new_target;
            self.transition_start = None;
            if let Err(e) = self.backend.apply(self.current_color) {
                error!("failed to apply display color: {e:#}");
            }
        } else {
            self.transition_start = Some(Instant::now());
        }
    }

    fn step_transition(&mut self) {
        let Some(started) = self.transition_start else {
            return;
        };
        let dur_ms = self.config.transition_ms.max(1) as f64;
        let t = (started.elapsed().as_secs_f64() * 1000.0) / dur_ms;
        let next = if t >= 1.0 {
            self.target_color
        } else {
            RenderColor::lerp(self.start_color, self.target_color, t)
        };
        match self.backend.apply(next) {
            Ok(()) => {
                self.current_color = next;
                if t >= 1.0 {
                    self.transition_start = None;
                }
            }
            Err(e) => {
                warn!("transient display apply error during transition: {e:#}");
                if t >= 2.0 {
                    // Stop retrying after generous grace window.
                    self.transition_start = None;
                }
            }
        }
    }

    fn save_config(&self) {
        if let Err(e) = self.config.save_to_path(&self.config_path) {
            warn!("failed to persist config: {e:#}");
        }
    }

    fn handle_request(&mut self, req: IpcRequest) -> (IpcResponse, RequestOutcome) {
        let mut outcome = RequestOutcome::Continue;
        let mut config_changed = false;
        let mut save_now = false;
        let mut instant = false;

        match req {
            IpcRequest::Status => {}
            IpcRequest::Set {
                temperature,
                intensity,
                brightness,
                enable,
            } => {
                if let Some(k) = temperature {
                    self.config.temperature = clamp_temperature(k);
                }
                if let Some(i) = intensity {
                    self.config.intensity = clamp_intensity(i);
                }
                if let Some(b) = brightness {
                    self.config.brightness = clamp_brightness(b);
                }
                if enable {
                    self.config.enabled = true;
                }
                config_changed = true;
                save_now = true;
            }
            IpcRequest::SetLive {
                temperature,
                intensity,
                brightness,
                enable,
            } => {
                if let Some(k) = temperature {
                    self.config.temperature = clamp_temperature(k);
                }
                if let Some(i) = intensity {
                    self.config.intensity = clamp_intensity(i);
                }
                if let Some(b) = brightness {
                    self.config.brightness = clamp_brightness(b);
                }
                if let Some(e) = enable {
                    self.config.enabled = e;
                }
                config_changed = true;
                instant = true;
            }
            IpcRequest::Persist => {
                save_now = true;
            }
            IpcRequest::Toggle => {
                self.config.enabled = !self.config.enabled;
                config_changed = true;
                save_now = true;
            }
            IpcRequest::SetEnabled { enabled } => {
                self.config.enabled = enabled;
                config_changed = true;
                save_now = true;
            }
            IpcRequest::Step { delta_k } => {
                let next = (self.config.temperature as i32) + delta_k;
                self.config.temperature = clamp_temperature(next.max(0) as u32);
                self.config.enabled = true;
                config_changed = true;
                save_now = true;
            }
            IpcRequest::SetSchedule { schedule } => {
                self.config.schedule = schedule;
                config_changed = true;
                save_now = true;
            }
            IpcRequest::ToggleControls => {
                outcome = RequestOutcome::ToggleControls;
            }
            IpcRequest::Quit => {
                outcome = RequestOutcome::Quit;
            }
        }

        if save_now {
            self.save_config();
        }
        if config_changed {
            self.retarget(instant);
        }

        (
            IpcResponse::Ok {
                data: self.snapshot(),
            },
            outcome,
        )
    }

    async fn smooth_reset_and_release(&mut self) {
        if !self.current_color.approx_eq(&RenderColor::NEUTRAL) {
            let ms = self.config.transition_ms.min(400);
            if ms > 0 {
                let start = self.current_color;
                let steps = 16usize;
                let step_dur = Duration::from_millis((ms / steps as u64).max(8));
                for i in 1..=steps {
                    let t = (i as f64) / (steps as f64);
                    let c = RenderColor::lerp(start, RenderColor::NEUTRAL, t);
                    let _ = self.backend.apply(c);
                    tokio::time::sleep(step_dur).await;
                }
            }
        }
        if let Err(e) = self.backend.reset() {
            warn!("failed to reset display cleanly: {e:#}");
        }
    }
}

pub async fn run_daemon(opts: DaemonOptions) -> Result<()> {
    let mut config = Config::load_from_path(&opts.config_path)?;
    if let Some(pref) = opts.backend_override {
        config.backend = pref;
    }

    let backend_pref = config.backend;
    let mut backend = tokio::task::spawn_blocking(move || create_backend(backend_pref))
        .await
        .map_err(|e| anyhow::anyhow!("backend init task panicked: {e}"))??;
    info!("pop-nightlight starting with backend: {}", backend.name());

    let listener: UnixListener = bind_listener(&opts.socket_path).await?;
    info!("IPC listening on {}", opts.socket_path.display());

    backend.apply(RenderColor::NEUTRAL)?;

    let mut state = DaemonState {
        config,
        config_path: opts.config_path,
        backend,
        current_color: RenderColor::NEUTRAL,
        start_color: RenderColor::NEUTRAL,
        target_color: RenderColor::NEUTRAL,
        transition_start: None,
    };
    state.retarget(false);

    let (status_tx, status_rx) = watch::channel(state.snapshot());
    let (cmd_tx, mut cmd_rx) =
        mpsc::unbounded_channel::<(IpcRequest, Option<oneshot::Sender<IpcResponse>>)>();

    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<IpcRequest>();
    let forward_tx = cmd_tx.clone();
    tokio::spawn(async move {
        while let Some(req) = ui_rx.recv().await {
            if forward_tx.send((req, None)).is_err() {
                break;
            }
        }
    });

    // Optional System Tray
    let tray_handle = if opts.no_tray {
        None
    } else {
        let tray = NightLightTray::new(state.snapshot(), ui_tx.clone());
        match tray.spawn().await {
            Ok(handle) => {
                info!("StatusNotifierItem tray registered");
                Some(handle)
            }
            Err(e) => {
                warn!("tray registration failed (continuing headless): {e:#}");
                None
            }
        }
    };

    // Accept IPC clients
    let ipc_tx = cmd_tx.clone();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((mut stream, _)) => {
                    let tx = ipc_tx.clone();
                    tokio::spawn(async move {
                        match read_request(&mut stream).await {
                            Ok(Some(req)) => {
                                let (resp_tx, resp_rx) = oneshot::channel();
                                if tx.send((req, Some(resp_tx))).is_ok()
                                    && let Ok(resp) = resp_rx.await
                                {
                                    let _ = write_response(&mut stream, &resp).await;
                                }
                            }
                            Ok(None) => {}
                            Err(e) => {
                                let _ = write_response(
                                    &mut stream,
                                    &IpcResponse::Error {
                                        message: e.to_string(),
                                    },
                                )
                                .await;
                            }
                        }
                    });
                }
                Err(e) => {
                    error!("IPC accept error: {e}");
                    break;
                }
            }
        }
    });

    let mut popover: Option<PopoverHandle> = if opts.open_controls_on_start {
        Some(PopoverHandle::spawn(status_rx.clone(), ui_tx.clone()))
    } else {
        None
    };
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut frame_tick = tokio::time::interval(Duration::from_millis(16));
    let mut backend_poll_tick = tokio::time::interval(Duration::from_millis(250));
    let mut schedule_tick = tokio::time::interval(Duration::from_secs(30));

    loop {
        tokio::select! {
            _ = sigint.recv() => {
                info!("received SIGINT, restoring 6500K and exiting");
                break;
            }
            _ = sigterm.recv() => {
                info!("received SIGTERM, restoring 6500K and exiting");
                break;
            }
            _ = frame_tick.tick(), if state.transition_start.is_some() => {
                state.step_transition();
            }
            _ = backend_poll_tick.tick() => {
                if let Err(e) = state.backend.poll_events() {
                    warn!("backend event poll error: {e:#}");
                }
            }
            _ = schedule_tick.tick() => {
                if state.config.schedule == ScheduleMode::SunsetToSunrise {
                    let before = state.target_color;
                    state.retarget(false);
                    if !before.approx_eq(&state.target_color) {
                        let snap = state.snapshot();
                        let _ = status_tx.send(snap.clone());
                        if let Some(h) = &tray_handle {
                            let _ = h.update(move |t| t.status = snap).await;
                        }
                    }
                }
            }
            Some((req, reply)) = cmd_rx.recv() => {
                let is_live = matches!(req, IpcRequest::SetLive { .. });
                let (resp, outcome) = state.handle_request(req);
                if let Some(tx) = reply {
                    let _ = tx.send(resp);
                }

                let snap = state.snapshot();
                let _ = status_tx.send(snap.clone());

                // Avoid flooding D-Bus tray updates on every 60Hz slider drag pixel;
                // tray updates immediately on release (Persist) and all non-live actions.
                if !is_live && let Some(h) = &tray_handle {
                    let _ = h.update(move |t| t.status = snap).await;
                }

                match outcome {
                    RequestOutcome::Continue => {}
                    RequestOutcome::ToggleControls => {
                        let was_open = popover.as_ref().is_some_and(PopoverHandle::is_open);
                        if let Some(mut old) = popover.take() {
                            old.close();
                        }
                        if !was_open {
                            popover = Some(PopoverHandle::spawn(status_rx.clone(), ui_tx.clone()));
                        }
                    }
                    RequestOutcome::Quit => {
                        info!("quit requested, restoring 6500K and exiting");
                        break;
                    }
                }
            }
        }
    }

    if let Some(mut pop) = popover.take() {
        pop.close();
    }
    state.smooth_reset_and_release().await;
    if let Some(h) = tray_handle {
        h.shutdown().await;
    }
    let _ = std::fs::remove_file(&opts.socket_path);
    Ok(())
}
