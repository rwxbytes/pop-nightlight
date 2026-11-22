use crate::config::ScheduleMode;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum IpcRequest {
    Status,
    Set {
        temperature: Option<u32>,
        intensity: Option<f64>,
        brightness: Option<f64>,
        #[serde(default = "default_true")]
        enable: bool,
    },
    /// Fast, non-persisted live update used while dragging sliders at 60Hz.
    SetLive {
        temperature: Option<u32>,
        intensity: Option<f64>,
        brightness: Option<f64>,
        enable: Option<bool>,
    },
    /// Flush current state to config.toml after slider drag completes.
    Persist,
    Toggle,
    SetEnabled {
        enabled: bool,
    },
    Step {
        delta_k: i32,
    },
    SetSchedule {
        schedule: ScheduleMode,
    },
    ToggleControls,
    Quit,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StatusSnapshot {
    pub active: bool,
    pub enabled: bool,
    pub temperature: u32,
    pub intensity: f64,
    pub brightness: f64,
    pub schedule: ScheduleMode,
    pub backend: String,
    pub solar_is_night: Option<bool>,
    pub next_transition: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum IpcResponse {
    Ok { data: StatusSnapshot },
    Error { message: String },
}

pub fn default_socket_path() -> PathBuf {
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(runtime_dir);
        if !p.as_os_str().is_empty() {
            return p.join("pop-nightlight.sock");
        }
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    std::env::temp_dir().join(format!("pop-nightlight-{user}.sock"))
}

pub async fn bind_listener(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            anyhow::bail!(
                "another pop-nightlight daemon is already running at {}",
                path.display()
            );
        }
        std::fs::remove_file(path)
            .with_context(|| format!("failed to remove stale socket {}", path.display()))?;
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create socket dir {}", parent.display()))?;
    }

    let listener = UnixListener::bind(path)
        .with_context(|| format!("failed to bind IPC socket {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to chmod IPC socket {}", path.display()))?;
    Ok(listener)
}

pub async fn send_request(path: &Path, req: &IpcRequest) -> Result<IpcResponse> {
    let stream = UnixStream::connect(path)
        .await
        .with_context(|| format!("failed to connect to daemon socket {}", path.display()))?;
    let (reader, mut writer) = stream.into_split();

    let mut payload = serde_json::to_string(req)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;
    writer.flush().await?;

    let mut lines = BufReader::new(reader).lines();
    let line = lines
        .next_line()
        .await?
        .context("daemon closed IPC connection without a response")?;
    let resp: IpcResponse =
        serde_json::from_str(&line).context("invalid JSON response from daemon")?;
    Ok(resp)
}

pub async fn read_request(stream: &mut UnixStream) -> Result<Option<IpcRequest>> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Ok(None);
    }
    let req: IpcRequest = serde_json::from_str(line.trim()).context("invalid IPC JSON request")?;
    Ok(Some(req))
}

pub async fn write_response(stream: &mut UnixStream, resp: &IpcResponse) -> Result<()> {
    let mut payload = serde_json::to_string(resp)?;
    payload.push('\n');
    stream.write_all(payload.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_sock_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("pop-nightlight-test-{name}-{nanos}.sock"))
    }

    #[tokio::test]
    async fn ipc_roundtrip() {
        let sock = temp_sock_path("rt");
        let listener = bind_listener(&sock).await.unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let req = read_request(&mut stream).await.unwrap().unwrap();
            assert!(matches!(req, IpcRequest::Toggle));
            let resp = IpcResponse::Ok {
                data: StatusSnapshot {
                    active: true,
                    enabled: true,
                    temperature: 3200,
                    intensity: 0.7,
                    brightness: 1.0,
                    schedule: ScheduleMode::Manual,
                    backend: "noop".to_string(),
                    solar_is_night: None,
                    next_transition: None,
                },
            };
            write_response(&mut stream, &resp).await.unwrap();
        });

        let resp = send_request(&sock, &IpcRequest::Toggle).await.unwrap();
        server.await.unwrap();
        let _ = std::fs::remove_file(&sock);

        match resp {
            IpcResponse::Ok { data } => {
                assert!(data.active);
                assert_eq!(data.temperature, 3200);
                assert!((data.intensity - 0.7).abs() < 1e-9);
            }
            IpcResponse::Error { message } => panic!("unexpected error: {message}"),
        }
    }
}
