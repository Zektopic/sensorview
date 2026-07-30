//! Sensor value formatting, shared by every front end.
//!
//! Lives outside `ui/` deliberately: the text report, the CLI and the TUI all
//! need it, and none of them should have to link the GUI toolkit to get it.
//! `ui::widgets` re-exports it so existing UI call sites are unchanged.

use crate::model::SensorType;

/// Format a sensor value with its unit, HWiNFO-style decimals.
///
/// `None` renders as an em dash: a sensor can be present without having
/// produced a reading this tick (see `model::Sensor::value`), and that is a
/// different thing from reading zero.
pub fn format_value(value: Option<f32>, t: SensorType) -> String {
    let Some(v) = value else { return "—".to_string() };
    let decimals = match t {
        SensorType::Voltage => 3,
        SensorType::Fan | SensorType::SmallData => 0,
        SensorType::Data => 0,
        _ => 1,
    };
    let unit = t.unit();
    if unit.is_empty() {
        format!("{v:.decimals$}")
    } else {
        format!("{v:.decimals$} {unit}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_readings_render_as_an_em_dash() {
        assert_eq!(format_value(None, SensorType::Temperature), "—");
        assert_eq!(format_value(None, SensorType::Power), "—");
    }

    #[test]
    fn decimals_and_units_follow_the_sensor_type() {
        assert_eq!(format_value(Some(1.234_5), SensorType::Voltage), "1.235 V");
        assert_eq!(format_value(Some(2964.0), SensorType::Clock), "2964.0 MHz");
        assert_eq!(format_value(Some(1834.6), SensorType::Fan), "1835 RPM");
        assert_eq!(format_value(Some(38.77), SensorType::Temperature), "38.8 °C");
    }

    /// `Factor` has no unit, so there must be no trailing space.
    #[test]
    fn unitless_types_have_no_trailing_space() {
        let s = format_value(Some(21.0), SensorType::Factor);
        assert_eq!(s, "21.0");
        assert!(!s.ends_with(' '));
    }
}
