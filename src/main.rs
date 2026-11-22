mod backend;
mod color;
mod config;
mod daemon;
mod ipc;
mod popover;
mod solar;
mod tray;

use crate::color::{clamp_brightness, clamp_intensity, clamp_temperature};
use crate::config::{BackendPreference, Config, ScheduleMode, default_config_path};
use crate::daemon::{DaemonOptions, run_daemon};
use crate::ipc::{IpcRequest, IpcResponse, StatusSnapshot, default_socket_path, send_request};
use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "pop-nightlight",
    version,
    about = "Lightweight pure-Rust Night Light with StatusNotifierItem tray & slider popover for Pop!_OS"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum BackendArg {
    Auto,
    Wayland,
    WaylandOverlay,
    Gnome,
    X11,
    Noop,
}

impl From<BackendArg> for BackendPreference {
    fn from(v: BackendArg) -> Self {
        match v {
            BackendArg::Auto => BackendPreference::Auto,
            BackendArg::Wayland => BackendPreference::Wayland,
            BackendArg::WaylandOverlay => BackendPreference::WaylandOverlay,
            BackendArg::Gnome => BackendPreference::Gnome,
            BackendArg::X11 => BackendPreference::X11,
            BackendArg::Noop => BackendPreference::Noop,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ScheduleArg {
    Manual,
    SunsetToSunrise,
}

impl From<ScheduleArg> for ScheduleMode {
    fn from(v: ScheduleArg) -> Self {
        match v {
            ScheduleArg::Manual => ScheduleMode::Manual,
            ScheduleArg::SunsetToSunrise => ScheduleMode::SunsetToSunrise,
        }
    }
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Run the Night Light daemon with system tray icon (default if no subcommand)
    Daemon {
        /// Force a specific display backend
        #[arg(long, value_enum)]
        backend: Option<BackendArg>,
        /// Run without registering a StatusNotifierItem tray icon
        #[arg(long)]
        no_tray: bool,
        /// Immediately open the COSMIC slider control popover on startup
        #[arg(long)]
        open_controls: bool,
    },
    /// Desktop app entrypoint: opens controls if daemon is running, or starts daemon + controls
    Launch,
    /// Set color temperature (1800..6500K), intensity (0.1..1.0), and/or brightness (0.3..1.0)
    Set {
        /// Color temperature in Kelvin (1800..6500)
        kelvin: u32,
        /// Optional filter intensity / warmth strength (0.1..1.0)
        #[arg(long)]
        intensity: Option<f64>,
        /// Optional brightness multiplier (0.3..1.0)
        #[arg(long)]
        brightness: Option<f64>,
    },
    /// Toggle Night Light on/off
    Toggle,
    /// Open or close the floating slider control panel
    Controls,
    /// Show current Night Light status
    Status {
        /// Output JSON instead of human-readable text
        #[arg(long)]
        json: bool,
    },
    /// Set schedule mode (manual or sunset-to-sunrise)
    Schedule {
        #[arg(value_enum)]
        mode: ScheduleArg,
    },
    /// Ask the running daemon to quit cleanly and restore 6500K
    Quit,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Commands::Daemon {
        backend: None,
        no_tray: false,
        open_controls: false,
    });

    match command {
        Commands::Daemon {
            backend,
            no_tray,
            open_controls,
        } => {
            let opts = DaemonOptions {
                config_path: default_config_path(),
                socket_path: default_socket_path(),
                backend_override: backend.map(Into::into),
                no_tray,
                open_controls_on_start: open_controls,
            };
            run_daemon(opts).await?;
        }
        Commands::Launch => {
            let sock = default_socket_path();
            if send_request(&sock, &IpcRequest::ToggleControls)
                .await
                .is_ok()
            {
                return Ok(());
            }
            let opts = DaemonOptions {
                config_path: default_config_path(),
                socket_path: sock,
                backend_override: None,
                no_tray: false,
                open_controls_on_start: true,
            };
            run_daemon(opts).await?;
        }
        Commands::Set {
            kelvin,
            intensity,
            brightness,
        } => {
            let k = clamp_temperature(kelvin);
            let req = IpcRequest::Set {
                temperature: Some(k),
                intensity: intensity.map(clamp_intensity),
                brightness: brightness.map(clamp_brightness),
                enable: true,
            };
            match send_or_update_config(&req, |cfg| {
                cfg.temperature = k;
                if let Some(i) = intensity {
                    cfg.intensity = clamp_intensity(i);
                }
                if let Some(b) = brightness {
                    cfg.brightness = clamp_brightness(b);
                }
                cfg.enabled = true;
            })
            .await?
            {
                Some(status) => print_status(&status, false),
                None => println!("Daemon not running; updated config to {k}K"),
            }
        }
        Commands::Toggle => {
            let req = IpcRequest::Toggle;
            match send_or_update_config(&req, |cfg| {
                cfg.enabled = !cfg.enabled;
            })
            .await?
            {
                Some(status) => print_status(&status, false),
                None => {
                    let cfg = Config::load_default()?;
                    println!(
                        "Daemon not running; updated config (enabled={})",
                        cfg.enabled
                    );
                }
            }
        }
        Commands::Controls => {
            let sock = default_socket_path();
            match send_request(&sock, &IpcRequest::ToggleControls).await {
                Ok(IpcResponse::Ok { data }) => print_status(&data, false),
                Ok(IpcResponse::Error { message }) => bail!("daemon error: {message}"),
                Err(e) => bail!("could not reach running daemon: {e:#}"),
            }
        }
        Commands::Schedule { mode } => {
            let schedule: ScheduleMode = mode.into();
            let req = IpcRequest::SetSchedule { schedule };
            match send_or_update_config(&req, |cfg| {
                cfg.schedule = schedule;
            })
            .await?
            {
                Some(status) => print_status(&status, false),
                None => println!("Daemon not running; updated schedule in config"),
            }
        }
        Commands::Status { json } => {
            let sock = default_socket_path();
            match send_request(&sock, &IpcRequest::Status).await {
                Ok(IpcResponse::Ok { data }) => print_status(&data, json),
                Ok(IpcResponse::Error { message }) => bail!("daemon error: {message}"),
                Err(_) => {
                    let cfg = Config::load_default()?;
                    if json {
                        let snap = StatusSnapshot {
                            active: false,
                            enabled: cfg.enabled,
                            temperature: cfg.temperature,
                            intensity: cfg.intensity,
                            brightness: cfg.brightness,
                            schedule: cfg.schedule,
                            backend: "offline (daemon not running)".to_string(),
                            solar_is_night: None,
                            next_transition: None,
                        };
                        println!("{}", serde_json::to_string_pretty(&snap)?);
                    } else {
                        println!(
                            "Daemon: not running (config: enabled={}, {}K, intensity={:.0}%, brightness={:.0}%, schedule={:?})",
                            cfg.enabled,
                            cfg.temperature,
                            cfg.intensity * 100.0,
                            cfg.brightness * 100.0,
                            cfg.schedule
                        );
                    }
                }
            }
        }
        Commands::Quit => {
            let sock = default_socket_path();
            match send_request(&sock, &IpcRequest::Quit).await {
                Ok(IpcResponse::Ok { .. }) => println!("Daemon stopping and restoring 6500K."),
                Ok(IpcResponse::Error { message }) => bail!("daemon error: {message}"),
                Err(e) => bail!("could not reach running daemon: {e:#}"),
            }
        }
    }

    Ok(())
}

async fn send_or_update_config<F>(req: &IpcRequest, update_fn: F) -> Result<Option<StatusSnapshot>>
where
    F: FnOnce(&mut Config),
{
    let sock = default_socket_path();
    if let Ok(resp) = send_request(&sock, req).await {
        return match resp {
            IpcResponse::Ok { data } => Ok(Some(data)),
            IpcResponse::Error { message } => bail!("daemon error: {message}"),
        };
    }

    let mut cfg = Config::load_default()?;
    update_fn(&mut cfg);
    cfg.save_default()?;
    Ok(None)
}

fn print_status(status: &StatusSnapshot, json: bool) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(status).unwrap_or_else(|_| "{}".to_string())
        );
        return;
    }

    let state = if status.active {
        format!("ON ({}K)", status.temperature)
    } else if status.enabled {
        "SCHEDULED (Daytime / Off)".to_string()
    } else {
        "OFF".to_string()
    };
    println!(
        "Night Light: {state} | Intensity: {:.0}% | Brightness: {:.0}% | Schedule: {:?} | Backend: {}",
        status.intensity * 100.0,
        status.brightness * 100.0,
        status.schedule,
        status.backend
    );
    if let Some(next) = &status.next_transition {
        println!("Next transition: {next}");
    }
}
