# pop-nightlight

Lightweight, pure-Rust Night Light daemon, CLI, `StatusNotifierItem` system tray, and COSMIC-styled floating slider popover for Pop!_OS (zero GTK).

## Features

- **Kelvin-to-RGB color temperature** (`1800K–6500K`, Tanner Helland curve) + filter **intensity** and **brightness** controls with smooth ~400ms transitions and clean `6500K` restore on exit.
- **Native COSMIC-style popover** (`zwlr_layer_shell_v1` + Fira Sans) with live-dragging sliders, presets (`2400K`, `3200K`, `4200K`, `6500K`), and `Manual` / `Sunset to Sunrise` NOAA solar schedule.
- **System tray (`StatusNotifierItem` via `ksni`)** with dynamic icon, tooltip, quick toggles, presets, and submenus.
- **CLI & Unix socket IPC** (`launch`, `daemon`, `controls`, `set`, `toggle`, `schedule`, `status`, `quit`).

## Backend Support & Testing Status

- **Tested:** `wayland-overlay` (`zwlr_layer_shell_v1` + `wp_viewporter` click-through overlay) — **only tested on Pop!_OS COSMIC 1.0.0** (`cosmic-comp 1.0.0`, which does not yet advertise `zwlr_gamma_control_manager_v1`).
- **Implemented (untested on hardware):**
  - Wayland hardware gamma (`zwlr_gamma_control_manager_v1`)
  - GNOME Color D-Bus (`org.gnome.SettingsDaemon.Color`)
  - Native X11 (`XRandR` via `x11rb`)

`backend = "auto"` tries them in order: `wayland` → `gnome` → `wayland_overlay` → `x11`.

## Install

```bash
git clone https://github.com/rwxbytes/pop-nightlight.git
cd pop-nightlight
cargo install --path .
```

### COSMIC Applications Launcher & Icon

```bash
mkdir -p ~/.local/share/applications ~/.local/share/icons/hicolor/scalable/apps
sed "s|^Exec=.*|Exec=$HOME/.cargo/bin/pop-nightlight launch|" \
  contrib/pop-nightlight.desktop > ~/.local/share/applications/pop-nightlight.desktop
cp contrib/pop-nightlight.svg ~/.local/share/icons/hicolor/scalable/apps/
update-desktop-database ~/.local/share/applications 2>/dev/null || true
```

### Optional `systemd --user` Autostart

```bash
mkdir -p ~/.config/systemd/user
cp contrib/pop-nightlight.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now pop-nightlight.service
```

## Usage

Search **Pop Night Light** in COSMIC Applications/Launcher, or use the CLI:

```bash
pop-nightlight launch                                   # start daemon + open sliders (or toggle sliders)
pop-nightlight daemon                                   # run background daemon + tray
pop-nightlight controls                                 # toggle slider popover
pop-nightlight set 3200 --intensity 0.7 --brightness 1  # set temperature/intensity/brightness
pop-nightlight toggle                                   # toggle on/off
pop-nightlight schedule sunset-to-sunrise               # manual | sunset-to-sunrise
pop-nightlight status [--json]                          # show current state
pop-nightlight quit                                     # restore 6500K and exit
```

## Config

Stored at `~/.config/pop-nightlight/config.toml`:

```toml
enabled = true
temperature = 3200
intensity = 0.7
brightness = 1.0
schedule = "manual" # manual | sunset_to_sunrise
latitude = 51.5074  # default: London, UK
longitude = -0.1278
transition_ms = 400
backend = "auto"    # auto | wayland | wayland_overlay | gnome | x11 | noop
```

## License

MIT
