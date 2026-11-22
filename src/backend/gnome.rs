use crate::backend::DisplayBackend;
use crate::color::{NEUTRAL_TEMP_K, RenderColor, clamp_temperature};
use anyhow::{Context, Result, bail};
use zbus::blocking::Connection;
use zbus::blocking::fdo::DBusProxy;

pub struct GnomeColorBackend {
    conn: Connection,
}

impl GnomeColorBackend {
    pub fn connect() -> Result<Self> {
        let conn = Connection::session().context("failed to connect to session D-Bus")?;
        let dbus = DBusProxy::new(&conn).context("failed to create DBusProxy")?;
        let has_owner = dbus
            .name_has_owner("org.gnome.SettingsDaemon.Color".try_into()?)
            .context("failed to query org.gnome.SettingsDaemon.Color owner")?;
        if !has_owner {
            bail!("org.gnome.SettingsDaemon.Color is not running on this session");
        }
        Ok(Self { conn })
    }

    fn set_temperature_u32(&self, kelvin: u32) -> Result<()> {
        let proxy = zbus::blocking::Proxy::new(
            &self.conn,
            "org.gnome.SettingsDaemon.Color",
            "/org/gnome/SettingsDaemon/Color",
            "org.gnome.SettingsDaemon.Color",
        )
        .context("failed to create GNOME Color proxy")?;
        proxy
            .set_property("Temperature", kelvin)
            .context("failed to set GNOME Color Temperature property")?;
        Ok(())
    }
}

impl DisplayBackend for GnomeColorBackend {
    fn name(&self) -> &'static str {
        "gnome (org.gnome.SettingsDaemon.Color)"
    }

    fn apply(&mut self, color: RenderColor) -> Result<()> {
        let multipliers = color.gamma_multipliers();
        if (multipliers[2] - 1.0).abs() < 1e-3 && (multipliers[1] - 1.0).abs() < 1e-3 {
            return self.set_temperature_u32(NEUTRAL_TEMP_K);
        }
        let approx_k = (1800.0 + multipliers[2].clamp(0.0, 1.0) * (6500.0 - 1800.0)).round() as u32;
        self.set_temperature_u32(clamp_temperature(approx_k))
    }

    fn reset(&mut self) -> Result<()> {
        self.set_temperature_u32(NEUTRAL_TEMP_K)
    }
}
