use crate::color::{
    DEFAULT_INTENSITY, DEFAULT_TEMP_K, MAX_BRIGHTNESS, clamp_brightness, clamp_intensity,
    clamp_temperature,
};
use crate::solar::{DEFAULT_LATITUDE, DEFAULT_LONGITUDE};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleMode {
    #[default]
    Manual,
    SunsetToSunrise,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BackendPreference {
    #[default]
    Auto,
    Wayland,
    WaylandOverlay,
    Gnome,
    X11,
    Noop,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub enabled: bool,
    pub temperature: u32,
    pub intensity: f64,
    pub brightness: f64,
    pub schedule: ScheduleMode,
    pub latitude: f64,
    pub longitude: f64,
    pub transition_ms: u64,
    pub backend: BackendPreference,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            temperature: DEFAULT_TEMP_K,
            intensity: DEFAULT_INTENSITY,
            brightness: MAX_BRIGHTNESS,
            schedule: ScheduleMode::Manual,
            latitude: DEFAULT_LATITUDE,
            longitude: DEFAULT_LONGITUDE,
            transition_ms: 400,
            backend: BackendPreference::Auto,
        }
    }
}

impl Config {
    pub fn normalized(mut self) -> Self {
        self.temperature = clamp_temperature(self.temperature);
        self.intensity = clamp_intensity(self.intensity);
        self.brightness = clamp_brightness(self.brightness);
        self.latitude = if self.latitude.is_nan() {
            DEFAULT_LATITUDE
        } else {
            self.latitude.clamp(-90.0, 90.0)
        };
        self.longitude = if self.longitude.is_nan() {
            DEFAULT_LONGITUDE
        } else {
            self.longitude.clamp(-180.0, 180.0)
        };
        self.transition_ms = self.transition_ms.min(5000);
        self
    }

    pub fn load_default() -> Result<Self> {
        Self::load_from_path(&default_config_path())
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read config at {}", path.display()))?;
        let parsed: Config = toml::from_str(&raw)
            .with_context(|| format!("failed to parse TOML config at {}", path.display()))?;
        Ok(parsed.normalized())
    }

    pub fn save_default(&self) -> Result<()> {
        self.save_to_path(&default_config_path())
    }

    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        let normalized = self.clone().normalized();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create config dir {}", parent.display()))?;
        }
        let encoded = toml::to_string_pretty(&normalized).context("failed to serialize config")?;
        let tmp_path = path.with_extension("toml.tmp");
        fs::write(&tmp_path, encoded)
            .with_context(|| format!("failed to write temp config {}", tmp_path.display()))?;
        fs::rename(&tmp_path, path)
            .with_context(|| format!("failed to rename temp config to {}", path.display()))?;
        Ok(())
    }
}

pub fn default_config_path() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        let p = PathBuf::from(xdg);
        if !p.as_os_str().is_empty() {
            return p.join("pop-nightlight").join("config.toml");
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join("pop-nightlight")
            .join("config.toml");
    }
    PathBuf::from("pop-nightlight.toml")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_config_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("pop-nightlight-test-{name}-{nanos}.toml"))
    }

    #[test]
    fn missing_file_returns_default() {
        let p = temp_config_path("missing");
        let cfg = Config::load_from_path(&p).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn roundtrip_and_clamping() {
        let p = temp_config_path("roundtrip");
        let cfg = Config {
            enabled: false,
            temperature: 9999,
            intensity: 5.0,
            brightness: 0.01,
            schedule: ScheduleMode::SunsetToSunrise,
            latitude: 53.4808,
            longitude: -2.2426,
            transition_ms: 500,
            backend: BackendPreference::Wayland,
        };
        cfg.save_to_path(&p).unwrap();
        let loaded = Config::load_from_path(&p).unwrap();
        let _ = fs::remove_file(&p);

        assert!(!loaded.enabled);
        assert_eq!(loaded.temperature, 6500);
        assert!((loaded.intensity - 1.0).abs() < 1e-9);
        assert!((loaded.brightness - 0.3).abs() < 1e-9);
        assert_eq!(loaded.schedule, ScheduleMode::SunsetToSunrise);
        assert!((loaded.latitude - 53.4808).abs() < 1e-6);
        assert!((loaded.longitude - (-2.2426)).abs() < 1e-6);
        assert_eq!(loaded.transition_ms, 500);
        assert_eq!(loaded.backend, BackendPreference::Wayland);
    }
}
