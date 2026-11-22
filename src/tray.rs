use crate::color::kelvin_to_rgb;
use crate::config::ScheduleMode;
use crate::ipc::{IpcRequest, StatusSnapshot};
use ksni::menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu};
use ksni::{Icon, MenuItem, ToolTip, Tray};
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct NightLightTray {
    pub status: StatusSnapshot,
    pub tx: mpsc::UnboundedSender<IpcRequest>,
}

impl NightLightTray {
    pub fn new(status: StatusSnapshot, tx: mpsc::UnboundedSender<IpcRequest>) -> Self {
        Self { status, tx }
    }

    fn send(&self, req: IpcRequest) {
        let _ = self.tx.send(req);
    }
}

/// Generate a crisp 24x24 ARGB32 icon (network byte order: A, R, G, B)
/// representing the current Night Light state and color temperature.
pub fn render_tray_icon(active: bool, temperature: u32) -> Icon {
    const SIZE: i32 = 24;
    let mut data = vec![0u8; (SIZE * SIZE * 4) as usize];

    let (r_f, g_f, b_f) = if active {
        let rgb = kelvin_to_rgb(temperature);
        (rgb[0], rgb[1] * 0.92, rgb[2] * 0.78)
    } else {
        (0.55, 0.62, 0.72)
    };

    let base_r = (r_f * 255.0).round().clamp(0.0, 255.0) as u8;
    let base_g = (g_f * 255.0).round().clamp(0.0, 255.0) as u8;
    let base_b = (b_f * 255.0).round().clamp(0.0, 255.0) as u8;

    let cx = 11.5_f64;
    let cy = 11.5_f64;
    let outer_r = 7.6_f64;
    let inner_cut_cx = 14.3_f64;
    let inner_cut_cy = 9.2_f64;
    let inner_cut_r = 5.6_f64;

    for y in 0..SIZE {
        for x in 0..SIZE {
            let fx = x as f64;
            let fy = y as f64;
            let dist = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();

            let alpha = if active {
                let disc_cov = (outer_r + 0.6 - dist).clamp(0.0, 1.0);
                let halo = if dist > outer_r && dist < 10.8 {
                    ((10.8 - dist) / 3.2) * 0.30
                } else {
                    0.0
                };
                disc_cov.max(halo)
            } else {
                let disc_cov = (outer_r + 0.5 - dist).clamp(0.0, 1.0);
                let cut_dist = ((fx - inner_cut_cx).powi(2) + (fy - inner_cut_cy).powi(2)).sqrt();
                let cut_cov = (cut_dist - (inner_cut_r - 0.5)).clamp(0.0, 1.0);
                (disc_cov * cut_cov).clamp(0.0, 1.0)
            };

            let a = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
            let idx = ((y * SIZE + x) * 4) as usize;
            data[idx] = a;
            data[idx + 1] = base_r;
            data[idx + 2] = base_g;
            data[idx + 3] = base_b;
        }
    }

    Icon {
        width: SIZE,
        height: SIZE,
        data,
    }
}

impl Tray for NightLightTray {
    fn id(&self) -> String {
        "pop-nightlight".to_string()
    }

    fn title(&self) -> String {
        if self.status.active {
            format!("Night Light: {}K", self.status.temperature)
        } else {
            "Night Light: Off".to_string()
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![render_tray_icon(
            self.status.active,
            self.status.temperature,
        )]
    }

    fn tool_tip(&self) -> ToolTip {
        let title = self.title();
        let sched = match self.status.schedule {
            ScheduleMode::Manual => "Mode: Manual".to_string(),
            ScheduleMode::SunsetToSunrise => {
                if let Some(next) = &self.status.next_transition {
                    format!("Mode: Sunset to Sunrise (next: {next})")
                } else {
                    "Mode: Sunset to Sunrise".to_string()
                }
            }
        };
        let description = format!(
            "{sched} • Intensity: {}% • Brightness: {}% • {}",
            (self.status.intensity * 100.0).round() as u32,
            (self.status.brightness * 100.0).round() as u32,
            self.status.backend
        );
        ToolTip {
            title,
            description,
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(IpcRequest::ToggleControls);
    }

    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        self.send(IpcRequest::Toggle);
    }

    fn scroll(&mut self, delta: i32, _dir: ksni::Orientation) {
        if delta != 0 {
            let step = if delta > 0 { 100 } else { -100 };
            self.send(IpcRequest::Step { delta_k: step });
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let selected_schedule = match self.status.schedule {
            ScheduleMode::Manual => 0,
            ScheduleMode::SunsetToSunrise => 1,
        };

        let int_pct = (self.status.intensity * 100.0).round() as u32;
        let bri_pct = (self.status.brightness * 100.0).round() as u32;

        let intensity_levels: &[(u32, f64, &str)] = &[
            (25, 0.25, "25% (Subtle)"),
            (50, 0.50, "50% (Moderate)"),
            (70, 0.70, "70% (Balanced)"),
            (85, 0.85, "85% (Strong)"),
            (100, 1.00, "100% (Maximum)"),
        ];
        let intensity_selected = intensity_levels
            .iter()
            .enumerate()
            .min_by_key(|(_, (p, _, _))| p.abs_diff(int_pct))
            .map(|(i, _)| i)
            .unwrap_or(2);

        let brightness_levels: &[(u32, f64, &str)] = &[
            (50, 0.50, "50%"),
            (70, 0.70, "70%"),
            (85, 0.85, "85%"),
            (100, 1.00, "100%"),
        ];
        let brightness_selected = brightness_levels
            .iter()
            .enumerate()
            .min_by_key(|(_, (p, _, _))| p.abs_diff(bri_pct))
            .map(|(i, _)| i)
            .unwrap_or(3);

        vec![
            CheckmarkItem {
                label: if self.status.active {
                    format!("Night Light On ({}K)", self.status.temperature)
                } else {
                    "Enable Night Light".to_string()
                },
                checked: self.status.enabled,
                activate: Box::new(|this: &mut Self| {
                    this.send(IpcRequest::Toggle);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Sliders & Controls…".to_string(),
                activate: Box::new(|this: &mut Self| {
                    this.send(IpcRequest::ToggleControls);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Warmest (2400K)".to_string(),
                activate: Box::new(|this: &mut Self| {
                    this.send(IpcRequest::Set {
                        temperature: Some(2400),
                        intensity: None,
                        brightness: None,
                        enable: true,
                    });
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Night (3200K)".to_string(),
                activate: Box::new(|this: &mut Self| {
                    this.send(IpcRequest::Set {
                        temperature: Some(3200),
                        intensity: None,
                        brightness: None,
                        enable: true,
                    });
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Evening (4200K)".to_string(),
                activate: Box::new(|this: &mut Self| {
                    this.send(IpcRequest::Set {
                        temperature: Some(4200),
                        intensity: None,
                        brightness: None,
                        enable: true,
                    });
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Daylight (6500K)".to_string(),
                activate: Box::new(|this: &mut Self| {
                    this.send(IpcRequest::Set {
                        temperature: Some(6500),
                        intensity: None,
                        brightness: None,
                        enable: true,
                    });
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Warmer (-200K)".to_string(),
                activate: Box::new(|this: &mut Self| {
                    this.send(IpcRequest::Step { delta_k: -200 });
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Cooler (+200K)".to_string(),
                activate: Box::new(|this: &mut Self| {
                    this.send(IpcRequest::Step { delta_k: 200 });
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            SubMenu {
                label: format!("Intensity ({int_pct}%)"),
                submenu: vec![
                    RadioGroup {
                        selected: intensity_selected,
                        select: Box::new(|this: &mut Self, idx| {
                            let val = match idx {
                                0 => 0.25,
                                1 => 0.50,
                                2 => 0.70,
                                3 => 0.85,
                                _ => 1.00,
                            };
                            this.send(IpcRequest::Set {
                                temperature: None,
                                intensity: Some(val),
                                brightness: None,
                                enable: true,
                            });
                        }),
                        options: intensity_levels
                            .iter()
                            .map(|(_, _, lbl)| RadioItem {
                                label: (*lbl).to_string(),
                                ..Default::default()
                            })
                            .collect(),
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: format!("Brightness ({bri_pct}%)"),
                submenu: vec![
                    RadioGroup {
                        selected: brightness_selected,
                        select: Box::new(|this: &mut Self, idx| {
                            let val = match idx {
                                0 => 0.50,
                                1 => 0.70,
                                2 => 0.85,
                                _ => 1.00,
                            };
                            this.send(IpcRequest::Set {
                                temperature: None,
                                intensity: None,
                                brightness: Some(val),
                                enable: true,
                            });
                        }),
                        options: brightness_levels
                            .iter()
                            .map(|(_, _, lbl)| RadioItem {
                                label: (*lbl).to_string(),
                                ..Default::default()
                            })
                            .collect(),
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            RadioGroup {
                selected: selected_schedule,
                select: Box::new(|this: &mut Self, idx| {
                    let schedule = if idx == 1 {
                        ScheduleMode::SunsetToSunrise
                    } else {
                        ScheduleMode::Manual
                    };
                    this.send(IpcRequest::SetSchedule { schedule });
                }),
                options: vec![
                    RadioItem {
                        label: "Schedule: Manual".to_string(),
                        ..Default::default()
                    },
                    RadioItem {
                        label: "Schedule: Sunset to Sunrise".to_string(),
                        ..Default::default()
                    },
                ],
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".to_string(),
                activate: Box::new(|this: &mut Self| {
                    this.send(IpcRequest::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_pixmap_has_valid_argb32_dimensions() {
        let on_icon = render_tray_icon(true, 3200);
        assert_eq!(on_icon.width, 24);
        assert_eq!(on_icon.height, 24);
        assert_eq!(on_icon.data.len(), 24 * 24 * 4);
        let center_idx = ((12 * 24 + 12) * 4) as usize;
        assert_eq!(on_icon.data[center_idx], 255);
        assert!(on_icon.data[center_idx + 1] > on_icon.data[center_idx + 3]);

        let off_icon = render_tray_icon(false, 3200);
        assert_eq!(off_icon.data.len(), 24 * 24 * 4);
    }
}
