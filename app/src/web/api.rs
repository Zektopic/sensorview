//! REST endpoints beside the WebSocket: one-shot JSON for scripts, a
//! Prometheus exposition for Grafana, and per-sensor history for charting.

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use super::AppState;
use crate::model::{Hardware, SensorType};

/// Liveness probe. Unauthenticated by design — it reveals no telemetry.
pub async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let frame = state.store.load();
    Json(json!({
        "status": "ok",
        "uptime_s": state.started.elapsed().as_secs(),
        "seq": frame.seq,
        "sensors": frame.sensor_count(),
        "source": frame.source,
    }))
}

/// The latest frame, verbatim.
///
/// Serves the string the poll thread already produced, so a `curl` costs no
/// serialization — the same bytes every WebSocket client received this tick.
pub async fn telemetry(State(state): State<AppState>) -> impl IntoResponse {
    let json = state.store.json();
    (
        [(header::CONTENT_TYPE, "application/json")],
        json.as_str().to_owned(),
    )
}

/// Static system inventory (CPU, board, memory modules, GPUs, drives, OS).
pub async fn system(State(state): State<AppState>) -> impl IntoResponse {
    // Clone out of the lock in one statement: the guard is dropped at the
    // semicolon, well before this function's only await point (its return).
    let info = state.sysinfo.read().ok().and_then(|g| g.clone());
    match info {
        Some(i) => Json(json!({ "ready": true, "system": i })),
        // The WMI/IOKit query runs in the background at startup.
        None => Json(json!({ "ready": false })),
    }
}

/// Retained samples for one sensor, oldest → newest.
pub async fn history(
    State(state): State<AppState>,
    Path(identifier): Path<String>,
) -> impl IntoResponse {
    let samples = state.store.history(&identifier);
    if samples.is_empty() && state.store.load().find_sensor(&identifier).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such sensor", "identifier": identifier })),
        );
    }
    (
        StatusCode::OK,
        Json(json!({
            "identifier": identifier,
            "count": samples.len(),
            "samples": samples,
        })),
    )
}

/// Prometheus text exposition, so the same data drops straight into Grafana.
pub async fn metrics(State(state): State<AppState>) -> impl IntoResponse {
    let frame = state.store.load();
    let mut out = String::with_capacity(8 * 1024);

    out.push_str("# HELP sensorview_up 1 when the sensor backend is producing frames.\n");
    out.push_str("# TYPE sensorview_up gauge\n");
    out.push_str(&format!("sensorview_up {}\n", u8::from(frame.seq > 0)));

    // One metric family per sensor type, so units stay consistent within a
    // family — Prometheus convention and a hard requirement for sane graphs.
    let mut families: std::collections::BTreeMap<SensorType, Vec<String>> = Default::default();
    collect(&frame.tree, None, &mut families);

    for (sensor_type, lines) in families {
        let name = metric_name(sensor_type);
        out.push_str(&format!(
            "# HELP {name} {} readings, in {}.\n# TYPE {name} gauge\n",
            type_label(sensor_type),
            unit_label(sensor_type),
        ));
        for line in lines {
            out.push_str(&line);
            out.push('\n');
        }
    }

    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        out,
    )
}

/// Flatten the tree into one exposition line per sensor with a reading.
fn collect(
    tree: &[Hardware],
    parent: Option<&str>,
    out: &mut std::collections::BTreeMap<SensorType, Vec<String>>,
) {
    for hw in tree {
        for s in &hw.sensors {
            let Some(v) = s.value else { continue };
            // Prometheus rejects NaN/Inf in the text format.
            if !v.is_finite() {
                continue;
            }
            let mut labels = format!(
                r#"hardware="{}",sensor="{}",id="{}""#,
                escape(&hw.name),
                escape(&s.name),
                escape(&s.identifier)
            );
            if let Some(p) = parent {
                labels.push_str(&format!(r#",parent="{}""#, escape(p)));
            }
            out.entry(s.sensor_type)
                .or_default()
                .push(format!("{}{{{}}} {}", metric_name(s.sensor_type), labels, v));
        }
        collect(&hw.sub_hardware, Some(&hw.name), out);
    }
}

/// Escape a label value per the Prometheus text format.
fn escape(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', "\\\"").replace('\n', r"\n")
}

/// Metric family name, suffixed with the base unit as Prometheus expects.
fn metric_name(t: SensorType) -> &'static str {
    match t {
        SensorType::Voltage => "sensorview_voltage_volts",
        SensorType::Current => "sensorview_current_amperes",
        SensorType::Clock => "sensorview_clock_megahertz",
        SensorType::Temperature => "sensorview_temperature_celsius",
        SensorType::Load => "sensorview_load_percent",
        SensorType::Frequency => "sensorview_frequency_hertz",
        SensorType::Fan => "sensorview_fan_rpm",
        SensorType::Flow => "sensorview_flow_liters_per_hour",
        SensorType::Control => "sensorview_control_percent",
        SensorType::Level => "sensorview_level_percent",
        SensorType::Factor => "sensorview_factor",
        SensorType::Power => "sensorview_power_watts",
        SensorType::Data => "sensorview_data_gigabytes",
        SensorType::SmallData => "sensorview_data_megabytes",
        SensorType::Throughput => "sensorview_throughput_megabytes_per_second",
        SensorType::TimeSpan => "sensorview_timespan_seconds",
        SensorType::Energy => "sensorview_energy_milliwatt_hours",
        SensorType::Noise => "sensorview_noise_decibels",
        SensorType::Conductivity => "sensorview_conductivity_microsiemens_per_cm",
        SensorType::Humidity => "sensorview_humidity_percent",
        _ => "sensorview_unknown",
    }
}

fn type_label(t: SensorType) -> &'static str {
    match t {
        SensorType::Voltage => "Voltage",
        SensorType::Current => "Current",
        SensorType::Clock => "Clock",
        SensorType::Temperature => "Temperature",
        SensorType::Load => "Load",
        SensorType::Frequency => "Frequency",
        SensorType::Fan => "Fan speed",
        SensorType::Flow => "Coolant flow",
        SensorType::Control => "Control",
        SensorType::Level => "Level",
        SensorType::Factor => "Factor",
        SensorType::Power => "Power",
        SensorType::Data => "Data",
        SensorType::SmallData => "Data",
        SensorType::Throughput => "Throughput",
        SensorType::TimeSpan => "Time span",
        SensorType::Energy => "Energy",
        SensorType::Noise => "Noise",
        SensorType::Conductivity => "Conductivity",
        SensorType::Humidity => "Humidity",
        _ => "Unknown",
    }
}

fn unit_label(t: SensorType) -> &'static str {
    let u = t.unit();
    if u.is_empty() {
        "arbitrary units"
    } else {
        u
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HardwareType, Sensor};
    use std::collections::BTreeMap;

    fn sensor(name: &str, t: SensorType, v: Option<f32>) -> Sensor {
        Sensor {
            identifier: format!("/cpu/0/{}/0", name.to_lowercase()),
            name: name.into(),
            sensor_type: t,
            index: 0,
            value: v,
            min: None,
            max: None,
            avg: None,
        }
    }

    fn tree() -> Vec<Hardware> {
        vec![Hardware {
            identifier: "/cpu/0".into(),
            name: r#"AMD "Ryzen" \ 9"#.into(), // deliberately hostile label
            hardware_type: HardwareType::Cpu,
            sensors: vec![
                sensor("Tctl", SensorType::Temperature, Some(52.5)),
                sensor("Package", SensorType::Power, Some(88.0)),
                sensor("Missing", SensorType::Power, None),
                sensor("Broken", SensorType::Load, Some(f32::NAN)),
            ],
            sub_hardware: vec![Hardware {
                identifier: "/cpu/0/sio".into(),
                name: "Nuvoton".into(),
                hardware_type: HardwareType::SuperIO,
                sensors: vec![sensor("CPU Fan", SensorType::Fan, Some(1200.0))],
                sub_hardware: vec![],
            }],
        }]
    }

    fn families() -> BTreeMap<SensorType, Vec<String>> {
        let mut out = BTreeMap::new();
        collect(&tree(), None, &mut out);
        out
    }

    #[test]
    fn groups_readings_into_one_family_per_sensor_type() {
        let f = families();
        assert_eq!(f[&SensorType::Temperature].len(), 1);
        assert_eq!(f[&SensorType::Power].len(), 1, "sensor with no reading is skipped");
        assert_eq!(f[&SensorType::Fan].len(), 1, "sub-hardware is included");
        assert!(!f.contains_key(&SensorType::Load), "NaN is not a valid sample");
    }

    #[test]
    fn sub_hardware_carries_its_parent_as_a_label() {
        let f = families();
        let fan = &f[&SensorType::Fan][0];
        assert!(fan.contains(r#"hardware="Nuvoton""#), "{fan}");
        assert!(fan.contains(r#"parent="AMD \"Ryzen\" \\ 9""#), "{fan}");
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(escape("line\nbreak"), r"line\nbreak");
        // The escaped hardware name must appear intact in the emitted line.
        let temp = &families()[&SensorType::Temperature][0];
        assert!(temp.starts_with("sensorview_temperature_celsius{"), "{temp}");
        assert!(temp.ends_with(" 52.5"), "{temp}");
    }

    #[test]
    fn metric_names_carry_their_base_unit() {
        assert_eq!(metric_name(SensorType::Temperature), "sensorview_temperature_celsius");
        assert_eq!(metric_name(SensorType::Power), "sensorview_power_watts");
        assert_eq!(metric_name(SensorType::Fan), "sensorview_fan_rpm");
        // Unitless quantities still need a HELP line that reads sensibly.
        assert_eq!(unit_label(SensorType::Factor), "arbitrary units");
        assert_eq!(unit_label(SensorType::Power), "W");
    }
}
