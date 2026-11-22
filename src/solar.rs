use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use std::f64::consts::PI;

/// Default coordinates: London, UK.
pub const DEFAULT_LATITUDE: f64 = 51.5074;
pub const DEFAULT_LONGITUDE: f64 = -0.1278;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolarState {
    Normal,
    MidnightSun,
    PolarNight,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolarTimes {
    pub sunrise: Option<DateTime<Utc>>,
    pub sunset: Option<DateTime<Utc>>,
    pub polar_state: PolarState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SolarStatus {
    pub is_night: bool,
    pub sunrise: Option<DateTime<Utc>>,
    pub sunset: Option<DateTime<Utc>>,
    /// Next transition `(timestamp_utc, will_be_night)`.
    pub next_transition: Option<(DateTime<Utc>, bool)>,
    pub polar_state: PolarState,
}

/// Calculate UTC sunrise and sunset for a given UTC date and geographic coordinates
/// using the NOAA Solar Calculation equations (zenith = 90.833°).
pub fn calculate_solar_times(date: NaiveDate, latitude: f64, longitude: f64) -> SolarTimes {
    let lat = latitude.clamp(-89.99, 89.99);
    let lon = longitude.clamp(-180.0, 180.0);

    let day_of_year = date.ordinal() as f64;
    let days_in_year = if date.leap_year() { 366.0 } else { 365.0 };

    // Fractional year in radians (evaluated at 12:00 UTC).
    let gamma = (2.0 * PI / days_in_year) * (day_of_year - 1.0);

    let eq_time = 229.18
        * (0.000075 + 0.001868 * gamma.cos()
            - 0.032077 * gamma.sin()
            - 0.014615 * (2.0 * gamma).cos()
            - 0.040849 * (2.0 * gamma).sin());

    let decl = 0.006918 - 0.399912 * gamma.cos() + 0.070257 * gamma.sin()
        - 0.006758 * (2.0 * gamma).cos()
        + 0.000907 * (2.0 * gamma).sin()
        - 0.002697 * (3.0 * gamma).cos()
        + 0.00148 * (3.0 * gamma).sin();

    let lat_rad = lat.to_radians();
    let zenith_rad = 90.833_f64.to_radians();

    let cos_ha = (zenith_rad.cos() / (lat_rad.cos() * decl.cos())) - lat_rad.tan() * decl.tan();

    if cos_ha > 1.0 {
        return SolarTimes {
            sunrise: None,
            sunset: None,
            polar_state: PolarState::PolarNight,
        };
    }
    if cos_ha < -1.0 {
        return SolarTimes {
            sunrise: None,
            sunset: None,
            polar_state: PolarState::MidnightSun,
        };
    }

    let ha_deg = cos_ha.acos().to_degrees();

    let sunrise_minutes = 720.0 - 4.0 * (lon + ha_deg) - eq_time;
    let sunset_minutes = 720.0 - 4.0 * (lon - ha_deg) - eq_time;

    let midnight = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).expect("valid midnight"));
    let sunrise = midnight + Duration::seconds((sunrise_minutes * 60.0).round() as i64);
    let sunset = midnight + Duration::seconds((sunset_minutes * 60.0).round() as i64);

    SolarTimes {
        sunrise: Some(sunrise),
        sunset: Some(sunset),
        polar_state: PolarState::Normal,
    }
}

/// Determine whether `now` is currently in the sunset-to-sunrise (night) window,
/// checking adjacent UTC days to handle arbitrary longitudes and midnight wrap-around.
pub fn solar_status_at(now: DateTime<Utc>, latitude: f64, longitude: f64) -> SolarStatus {
    let today = now.date_naive();
    let yesterday = today - Duration::days(1);
    let tomorrow = today + Duration::days(1);

    let t_yest = calculate_solar_times(yesterday, latitude, longitude);
    let t_today = calculate_solar_times(today, latitude, longitude);
    let t_tom = calculate_solar_times(tomorrow, latitude, longitude);

    if t_today.polar_state == PolarState::PolarNight {
        return SolarStatus {
            is_night: true,
            sunrise: None,
            sunset: None,
            next_transition: None,
            polar_state: PolarState::PolarNight,
        };
    }
    if t_today.polar_state == PolarState::MidnightSun {
        return SolarStatus {
            is_night: false,
            sunrise: None,
            sunset: None,
            next_transition: None,
            polar_state: PolarState::MidnightSun,
        };
    }

    // Collect all sunrise/sunset events across [yesterday, today, tomorrow].
    // Each event is (timestamp, becomes_night).
    let mut events: Vec<(DateTime<Utc>, bool)> = Vec::with_capacity(6);
    for st in [&t_yest, &t_today, &t_tom] {
        if let Some(sr) = st.sunrise {
            events.push((sr, false));
        }
        if let Some(ss) = st.sunset {
            events.push((ss, true));
        }
    }
    events.sort_by_key(|(ts, _)| *ts);

    let mut is_night = false;
    for (ts, becomes_night) in &events {
        if *ts <= now {
            is_night = *becomes_night;
        }
    }

    let next_transition = events.into_iter().find(|(ts, _)| *ts > now);

    SolarStatus {
        is_night,
        sunrise: t_today.sunrise,
        sunset: t_today.sunset,
        next_transition,
        polar_state: PolarState::Normal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    #[test]
    fn london_equinox_and_solstice_times() {
        // March 20 (vernal equinox): sunrise ~06:05 UTC, sunset ~18:13 UTC
        let eq = NaiveDate::from_ymd_opt(2025, 3, 20).unwrap();
        let st = calculate_solar_times(eq, DEFAULT_LATITUDE, DEFAULT_LONGITUDE);
        assert_eq!(st.polar_state, PolarState::Normal);
        let sr = st.sunrise.unwrap();
        let ss = st.sunset.unwrap();
        assert_eq!(sr.hour(), 6);
        assert_eq!(ss.hour(), 18);

        // June 21 (summer solstice): sunrise ~03:43 UTC, sunset ~20:21 UTC
        let jun = NaiveDate::from_ymd_opt(2025, 6, 21).unwrap();
        let st_jun = calculate_solar_times(jun, DEFAULT_LATITUDE, DEFAULT_LONGITUDE);
        let sr_jun = st_jun.sunrise.unwrap();
        let ss_jun = st_jun.sunset.unwrap();
        assert!(sr_jun.hour() == 3 || sr_jun.hour() == 4);
        assert_eq!(ss_jun.hour(), 20);

        // Dec 21 (winter solstice): sunrise ~08:03 UTC, sunset ~15:53 UTC
        let dec = NaiveDate::from_ymd_opt(2025, 12, 21).unwrap();
        let st_dec = calculate_solar_times(dec, DEFAULT_LATITUDE, DEFAULT_LONGITUDE);
        let sr_dec = st_dec.sunrise.unwrap();
        let ss_dec = st_dec.sunset.unwrap();
        assert!(sr_dec.hour() == 7 || sr_dec.hour() == 8);
        assert!(ss_dec.hour() == 15 || ss_dec.hour() == 16);
    }

    #[test]
    fn day_and_night_detection_london() {
        let day = Utc.with_ymd_and_hms(2025, 6, 21, 12, 0, 0).unwrap();
        let night_late = Utc.with_ymd_and_hms(2025, 6, 21, 23, 0, 0).unwrap();
        let night_early = Utc.with_ymd_and_hms(2025, 6, 21, 2, 0, 0).unwrap();

        assert!(!solar_status_at(day, DEFAULT_LATITUDE, DEFAULT_LONGITUDE).is_night);
        assert!(solar_status_at(night_late, DEFAULT_LATITUDE, DEFAULT_LONGITUDE).is_night);
        assert!(solar_status_at(night_early, DEFAULT_LATITUDE, DEFAULT_LONGITUDE).is_night);
    }

    #[test]
    fn polar_edge_cases() {
        // Svalbard in June -> MidnightSun
        let jun = NaiveDate::from_ymd_opt(2025, 6, 21).unwrap();
        let svalbard_jun = calculate_solar_times(jun, 78.22, 15.63);
        assert_eq!(svalbard_jun.polar_state, PolarState::MidnightSun);

        // Svalbard in December -> PolarNight
        let dec = NaiveDate::from_ymd_opt(2025, 12, 21).unwrap();
        let svalbard_dec = calculate_solar_times(dec, 78.22, 15.63);
        assert_eq!(svalbard_dec.polar_state, PolarState::PolarNight);
    }
}
