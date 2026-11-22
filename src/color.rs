pub const MIN_TEMP_K: u32 = 1800;
pub const MAX_TEMP_K: u32 = 6500;
pub const DEFAULT_TEMP_K: u32 = 3400;
pub const NEUTRAL_TEMP_K: u32 = 6500;

pub const MIN_INTENSITY: f64 = 0.1;
pub const MAX_INTENSITY: f64 = 1.0;
pub const DEFAULT_INTENSITY: f64 = 0.7;

pub const MIN_BRIGHTNESS: f64 = 0.3;
pub const MAX_BRIGHTNESS: f64 = 1.0;

pub const NEUTRAL_RGB: [f64; 3] = [1.0, 1.0, 1.0];

/// Complete visual state passed to display backends and interpolated smoothly
/// during ~400ms transitions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderColor {
    /// Base Tanner Helland blackbody RGB multipliers [0.0..=1.0; 3].
    pub rgb: [f64; 3],
    /// Filter intensity / warmth strength [0.0..=1.0]. (0.0 when neutral/off.)
    pub intensity: f64,
    /// Display brightness multiplier [0.1..=1.0].
    pub brightness: f64,
}

impl RenderColor {
    pub const NEUTRAL: Self = Self {
        rgb: NEUTRAL_RGB,
        intensity: 0.0,
        brightness: MAX_BRIGHTNESS,
    };

    pub fn from_settings(kelvin: u32, intensity: f64, brightness: f64) -> Self {
        Self {
            rgb: kelvin_to_rgb(kelvin),
            intensity: clamp_intensity(intensity),
            brightness: clamp_brightness(brightness),
        }
    }

    pub fn approx_eq(&self, other: &Self) -> bool {
        (self.rgb[0] - other.rgb[0]).abs() <= 1e-5
            && (self.rgb[1] - other.rgb[1]).abs() <= 1e-5
            && (self.rgb[2] - other.rgb[2]).abs() <= 1e-5
            && (self.intensity - other.intensity).abs() <= 1e-5
            && (self.brightness - other.brightness).abs() <= 1e-5
    }

    pub fn lerp(from: Self, to: Self, t: f64) -> Self {
        let s = smoothstep(t);
        Self {
            rgb: [
                from.rgb[0] + (to.rgb[0] - from.rgb[0]) * s,
                from.rgb[1] + (to.rgb[1] - from.rgb[1]) * s,
                from.rgb[2] + (to.rgb[2] - from.rgb[2]) * s,
            ],
            intensity: from.intensity + (to.intensity - from.intensity) * s,
            brightness: from.brightness + (to.brightness - from.brightness) * s,
        }
    }

    /// Final per-channel RGB multipliers for hardware gamma LUT backends
    /// (`zwlr_gamma_control_v1`, XRandR).
    pub fn gamma_multipliers(&self) -> [f64; 3] {
        let i = self.intensity.clamp(0.0, 1.0);
        let b = self.brightness.clamp(0.0, 1.0);
        [
            (1.0 + (self.rgb[0] - 1.0) * i) * b,
            (1.0 + (self.rgb[1] - 1.0) * i) * b,
            (1.0 + (self.rgb[2] - 1.0) * i) * b,
        ]
    }

    /// Premultiplied ARGB8888 `[A, R, G, B]` pixel for the Wayland
    /// `zwlr_layer_shell_v1` click-through overlay backend.
    ///
    /// Guarantees the Wayland premultiplied alpha invariant (`r <= a`, `g <= a`, `b <= a`).
    pub fn overlay_argb(&self) -> [u8; 4] {
        let b = self.brightness.clamp(0.0, 1.0);
        let intensity = self.intensity.clamp(0.0, 1.0);
        let ng = self.rgb[1].clamp(0.0, 1.0);
        let nb = self.rgb[2].clamp(0.0, 1.0);

        let warmth = (1.0 - nb).clamp(0.0, 1.0);
        let a_tint = (warmth * 0.62 * intensity).clamp(0.0, 0.72);
        let tr = 0.90;
        let tg = if warmth > 1e-4 {
            (((ng - nb) / warmth) * ng * 0.85).clamp(0.0, 0.85)
        } else {
            0.0
        };
        let tb = 0.0;

        let alpha = (1.0 - b * (1.0 - a_tint)).clamp(0.0, 1.0);
        let r_pre = (b * a_tint * tr).clamp(0.0, alpha);
        let g_pre = (b * a_tint * tg).clamp(0.0, alpha);
        let b_pre = (b * a_tint * tb).clamp(0.0, alpha);

        let a_u8 = (alpha * 255.0).round().clamp(0.0, 255.0) as u8;
        let r_u8 = (r_pre * 255.0).round().clamp(0.0, a_u8 as f64) as u8;
        let g_u8 = (g_pre * 255.0).round().clamp(0.0, a_u8 as f64) as u8;
        let b_u8 = (b_pre * 255.0).round().clamp(0.0, a_u8 as f64) as u8;
        [a_u8, r_u8, g_u8, b_u8]
    }
}

/// Clamp a color temperature in Kelvin to the supported range [1800, 6500].
#[inline]
pub fn clamp_temperature(kelvin: u32) -> u32 {
    kelvin.clamp(MIN_TEMP_K, MAX_TEMP_K)
}

/// Clamp a filter intensity multiplier to [0.1, 1.0].
#[inline]
pub fn clamp_intensity(intensity: f64) -> f64 {
    if intensity.is_nan() {
        return DEFAULT_INTENSITY;
    }
    intensity.clamp(MIN_INTENSITY, MAX_INTENSITY)
}

/// Clamp a brightness multiplier to [0.3, 1.0].
#[inline]
pub fn clamp_brightness(brightness: f64) -> f64 {
    if brightness.is_nan() {
        return MAX_BRIGHTNESS;
    }
    brightness.clamp(MIN_BRIGHTNESS, MAX_BRIGHTNESS)
}

/// Convert a color temperature in Kelvin (1800K..=6500K) to normalized linear-ish
/// RGB multipliers [0.0..=1.0; 3] using Tanner Helland's blackbody approximation.
///
/// Temperatures >= 6500K return exact neutral `[1.0, 1.0, 1.0]`.
pub fn kelvin_to_rgb(kelvin: u32) -> [f64; 3] {
    let k = clamp_temperature(kelvin);
    if k >= NEUTRAL_TEMP_K {
        return NEUTRAL_RGB;
    }

    let temp = (k as f64) / 100.0;

    let red = 255.0_f64;
    let green = 99.4708025861 * temp.ln() - 161.1195681661;
    let blue = if temp <= 19.0 {
        0.0
    } else {
        138.5177312231 * (temp - 10.0).ln() - 305.0447927307
    };

    [
        (red / 255.0).clamp(0.0, 1.0),
        (green / 255.0).clamp(0.0, 1.0),
        (blue / 255.0).clamp(0.0, 1.0),
    ]
}

/// Build a planar 16-bit gamma LUT: [R_0..R_{n-1}, G_0..G_{n-1}, B_0..B_{n-1}].
pub fn build_gamma_lut(size: usize, multipliers: [f64; 3]) -> Vec<u16> {
    if size == 0 {
        return Vec::new();
    }
    let mut lut = vec![0u16; size * 3];
    let denom = if size <= 1 { 1.0 } else { (size - 1) as f64 };

    for (ch, &raw_mult) in multipliers.iter().enumerate() {
        let mult = raw_mult.clamp(0.0, 1.0);
        let offset = ch * size;
        for i in 0..size {
            let frac = if size <= 1 { 1.0 } else { (i as f64) / denom };
            let v = (frac * mult * 65535.0).round().clamp(0.0, 65535.0) as u16;
            lut[offset + i] = v;
        }
    }

    lut
}

/// Smoothstep easing curve for [0.0, 1.0].
#[inline]
pub fn smoothstep(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_6500k_is_identity() {
        assert_eq!(kelvin_to_rgb(6500), [1.0, 1.0, 1.0]);
        assert_eq!(kelvin_to_rgb(9000), [1.0, 1.0, 1.0]);
        assert_eq!(RenderColor::NEUTRAL.gamma_multipliers(), [1.0, 1.0, 1.0]);
        assert_eq!(RenderColor::NEUTRAL.overlay_argb(), [0, 0, 0, 0]);
    }

    #[test]
    fn warm_temperatures_reduce_blue_and_green_monotonically() {
        let k1800 = kelvin_to_rgb(1800);
        let k2400 = kelvin_to_rgb(2400);
        let k3200 = kelvin_to_rgb(3200);
        let k4200 = kelvin_to_rgb(4200);
        let k6500 = kelvin_to_rgb(6500);

        for rgb in [k1800, k2400, k3200, k4200, k6500] {
            assert!((rgb[0] - 1.0).abs() < 1e-6);
        }

        assert_eq!(k1800[2], 0.0);
        assert!(
            k1800[1] < k2400[1]
                && k2400[1] < k3200[1]
                && k3200[1] < k4200[1]
                && k4200[1] < k6500[1]
        );
        assert!(
            k1800[2] < k2400[2]
                && k2400[2] < k3200[2]
                && k3200[2] < k4200[2]
                && k4200[2] < k6500[2]
        );
    }

    #[test]
    fn tanner_helland_reference_values() {
        let rgb = kelvin_to_rgb(3200);
        assert!((rgb[0] - 1.0).abs() < 1e-6);
        assert!((rgb[1] - 0.720).abs() < 0.01);
        assert!((rgb[2] - 0.483).abs() < 0.01);
    }

    #[test]
    fn brightness_and_intensity_scale_and_clamp() {
        let full = RenderColor::from_settings(3400, 1.0, 1.0).gamma_multipliers();
        let half_b = RenderColor::from_settings(3400, 1.0, 0.5).gamma_multipliers();
        for ch in 0..3 {
            assert!((half_b[ch] - full[ch] * 0.5).abs() < 1e-9);
        }

        let clamped_low = RenderColor::from_settings(3400, 0.0, 0.0);
        assert!((clamped_low.intensity - MIN_INTENSITY).abs() < 1e-9);
        assert!((clamped_low.brightness - MIN_BRIGHTNESS).abs() < 1e-9);
    }

    #[test]
    fn gamma_lut_identity_and_scaled() {
        let size = 256;
        let lut = build_gamma_lut(size, NEUTRAL_RGB);
        assert_eq!(lut.len(), size * 3);
        assert_eq!(lut[0], 0);
        assert_eq!(lut[size - 1], 65535);
        assert_eq!(lut[3 * size - 1], 65535);

        let warm_lut = build_gamma_lut(size, [1.0, 0.5, 0.25]);
        assert_eq!(warm_lut[size - 1], 65535);
        assert!((warm_lut[2 * size - 1] as i32 - 32768).abs() <= 1);
        assert!((warm_lut[3 * size - 1] as i32 - 16384).abs() <= 1);
    }

    #[test]
    fn overlay_argb_scales_with_intensity_and_is_premultiplied() {
        let low = RenderColor::from_settings(3200, 0.25, 1.0).overlay_argb();
        let mid = RenderColor::from_settings(3200, 0.70, 1.0).overlay_argb();
        let high = RenderColor::from_settings(3200, 1.00, 1.0).overlay_argb();
        assert!(low[0] < mid[0] && mid[0] < high[0]);
        for [a, r, g, b] in [low, mid, high] {
            assert!(r <= a && g <= a && b <= a);
            assert!(r > g && g >= b);
        }

        let dim = RenderColor::from_settings(6500, 0.7, 0.5).overlay_argb();
        assert!((dim[0] as i32 - 128).abs() <= 1);
        assert_eq!([dim[1], dim[2], dim[3]], [0, 0, 0]);
    }

    #[test]
    fn render_color_lerp_endpoints_and_midpoint() {
        let a = RenderColor::from_settings(2400, 0.8, 0.8);
        let b = RenderColor::NEUTRAL;
        assert!(RenderColor::lerp(a, b, 0.0).approx_eq(&a));
        assert!(RenderColor::lerp(a, b, 1.0).approx_eq(&b));
        let mid = RenderColor::lerp(a, b, 0.5);
        assert!((mid.intensity - 0.4).abs() < 1e-9);
        assert!((mid.brightness - 0.9).abs() < 1e-9);
    }
}
