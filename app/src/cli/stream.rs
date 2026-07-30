//! Continuous telemetry on stdout, for piping.
//!
//! Reads `store.load()` and serialises with `serde_json` rather than using the
//! `broadcast` fan-out in `state.rs`: that channel is `feature = "web"` because
//! it is `tokio::sync::broadcast`, and streaming must work in a build with no
//! tokio at all.
//!
//! The CSV writer here is **not** `logging::CsvLogger`. That type freezes its
//! column set at construction and its "Time" column is a row counter, not a
//! timestamp (`logging.rs`) — both wrong for a stream someone is piping into
//! another tool.

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use crate::model::{Hardware, Sensor};
use crate::runtime::Runtime;
use crate::state::TelemetryFrame;

use super::render::matches;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// One JSON object per line — the whole frame, as `/api/telemetry` returns.
    Ndjson,
    /// Timestamped CSV with one column per sensor.
    Csv,
}

pub fn run(
    rt: &Runtime,
    format: Format,
    filter: Option<String>,
    count: Option<u64>,
    timeout: Duration,
) -> ExitCode {
    // Wait for a frame that can actually carry rate-derived sensors, so the
    // first line out isn't missing power and clocks.
    let first = match rt.wait_for_usable_frame(timeout) {
        Ok(f) => f,
        Err(f) if f.seq > 0 => {
            eprintln!("sensorview: warning — streaming from frame {}; rate-based sensors may be missing.", f.seq);
            f
        }
        Err(_) => {
            eprintln!("sensorview: no sensor data after {timeout:?} — is a backend available?");
            return ExitCode::FAILURE;
        }
    };

    let needle = filter.map(|f| f.to_lowercase());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    // CSV columns are fixed from the first frame so the file stays rectangular;
    // a sensor appearing later would otherwise shift every subsequent row.
    let columns: Vec<String> = if format == Format::Csv {
        let cols = column_ids(&first, needle.as_deref());
        if write_line(&mut out, &csv_header(&first, &cols)).is_err() {
            return ExitCode::SUCCESS;
        }
        cols
    } else {
        Vec::new()
    };

    let mut emitted = 0u64;
    let mut last_seq = 0u64;
    loop {
        let frame = rt.store.load();
        // Emit once per published frame rather than on a timer of our own, so
        // the stream cadence matches the poll cadence exactly.
        if frame.seq == last_seq {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        last_seq = frame.seq;

        let line = match format {
            Format::Ndjson => ndjson_line(&frame, needle.as_deref()),
            Format::Csv => csv_row(&frame, &columns),
        };
        // A closed pipe is the normal end of `sensorview stream | head -5`.
        // Rust sets SIGPIPE to ignore, so the write returns EPIPE instead of
        // terminating the process; treat it as the consumer leaving.
        if write_line(&mut out, &line).is_err() {
            return ExitCode::SUCCESS;
        }

        emitted += 1;
        if count.is_some_and(|n| emitted >= n) {
            return ExitCode::SUCCESS;
        }
    }
}

fn write_line(out: &mut impl Write, line: &str) -> std::io::Result<()> {
    writeln!(out, "{line}")?;
    // Flushed every line: a stream nobody can read until the buffer fills is
    // useless in a pipeline.
    out.flush()
}

/// The whole frame as one JSON object, or a filtered subset.
pub fn ndjson_line(frame: &TelemetryFrame, needle: Option<&str>) -> String {
    let Some(needle) = needle else {
        return serde_json::to_string(frame)
            .unwrap_or_else(|e| format!(r#"{{"error":"{e}"}}"#));
    };
    // With a filter, emit a compact record instead of the full tree — the
    // point of filtering is to reduce what downstream has to parse.
    let mut sensors = Vec::new();
    collect(&frame.tree, Some(needle), &mut |_hw, s| {
        sensors.push(serde_json::json!({
            "identifier": s.identifier,
            "name": s.name,
            "type": s.sensor_type,
            "unit": s.sensor_type.unit(),
            "value": s.value,
        }));
    });
    serde_json::json!({
        "seq": frame.seq,
        "unix_ms": frame.unix_ms,
        "sensors": sensors,
    })
    .to_string()
}

/// Stable identifiers for the CSV columns, in tree order.
pub fn column_ids(frame: &TelemetryFrame, needle: Option<&str>) -> Vec<String> {
    let mut ids = Vec::new();
    collect(&frame.tree, needle, &mut |_hw, s| ids.push(s.identifier.clone()));
    ids
}

/// `unix_ms,<Hardware Sensor [unit]>,…`
pub fn csv_header(frame: &TelemetryFrame, columns: &[String]) -> String {
    let mut labels: Vec<String> = Vec::with_capacity(columns.len());
    collect(&frame.tree, None, &mut |hw, s| {
        if columns.iter().any(|c| c == &s.identifier) {
            let unit = s.sensor_type.unit();
            let label = if unit.is_empty() {
                format!("{} {}", hw, s.name)
            } else {
                format!("{} {} [{}]", hw, s.name, unit)
            };
            labels.push(escape_csv(&label));
        }
    });
    format!("unix_ms,{}", labels.join(","))
}

/// One row, values in `columns` order. A sensor with no reading this tick
/// leaves its field empty rather than writing 0.
pub fn csv_row(frame: &TelemetryFrame, columns: &[String]) -> String {
    let mut by_id = std::collections::HashMap::new();
    collect(&frame.tree, None, &mut |_hw, s| {
        by_id.insert(s.identifier.as_str(), s.value);
    });
    let mut row = String::with_capacity(columns.len() * 8 + 16);
    row.push_str(&frame.unix_ms.to_string());
    for id in columns {
        row.push(',');
        if let Some(Some(v)) = by_id.get(id.as_str()) {
            row.push_str(&format!("{v:.3}"));
        }
    }
    row
}

/// Walk every sensor, passing the owning hardware's name alongside it.
///
/// The explicit `'a` ties the yielded references to `tree`, so a caller can
/// stash them in a map that outlives the closure body.
fn collect<'a>(
    tree: &'a [Hardware],
    needle: Option<&str>,
    f: &mut impl FnMut(&'a str, &'a Sensor),
) {
    for hw in tree {
        for s in &hw.sensors {
            if needle.is_none_or(|n| matches(s, n)) {
                f(&hw.name, s);
            }
        }
        collect(&hw.sub_hardware, needle, f);
    }
}

/// Quote a CSV field if it contains a comma, quote or newline.
fn escape_csv(field: &str) -> String {
    if field.contains([',', '"', '\n']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HardwareType, SensorType};

    fn s(name: &str, id: &str, ty: SensorType, value: Option<f32>) -> Sensor {
        Sensor {
            identifier: id.into(),
            name: name.into(),
            sensor_type: ty,
            index: 0,
            value,
            min: None,
            max: None,
            avg: None,
        }
    }

    fn frame() -> TelemetryFrame {
        TelemetryFrame {
            seq: 3,
            unix_ms: 1_738_000_000_123,
            tree: vec![Hardware {
                identifier: "/cpu/0".into(),
                name: "Apple M5".into(),
                hardware_type: HardwareType::Cpu,
                sensors: vec![
                    s("CPU Package Power", "/p", SensorType::Power, Some(21.5)),
                    s("Quiet", "/q", SensorType::Temperature, None),
                    s("Comma, Name", "/c", SensorType::Load, Some(5.0)),
                ],
                sub_hardware: Vec::new(),
            }],
            ..Default::default()
        }
    }

    /// The whole reason this doesn't reuse CsvLogger: a stream needs a real
    /// timestamp, not a row counter.
    #[test]
    fn csv_first_column_is_a_unix_timestamp() {
        let cols = column_ids(&frame(), None);
        let header = csv_header(&frame(), &cols);
        assert!(header.starts_with("unix_ms,"), "{header}");
        let row = csv_row(&frame(), &cols);
        assert!(row.starts_with("1738000000123,"), "{row}");
    }

    /// Split a CSV line into fields, honouring quotes — counting commas
    /// naively would miscount any field that legitimately contains one.
    fn fields(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut in_quotes = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '"' if in_quotes && chars.peek() == Some(&'"') => {
                    cur.push('"');
                    chars.next();
                }
                '"' => in_quotes = !in_quotes,
                ',' if !in_quotes => out.push(std::mem::take(&mut cur)),
                other => cur.push(other),
            }
        }
        out.push(cur);
        out
    }

    #[test]
    fn csv_row_matches_header_width_and_leaves_gaps_empty() {
        let cols = column_ids(&frame(), None);
        let header = fields(&csv_header(&frame(), &cols));
        let row = fields(&csv_row(&frame(), &cols));
        // Rectangular output: a reader must be able to zip header to row.
        assert_eq!(header.len(), row.len(), "header {header:?} vs row {row:?}");
        assert_eq!(header.len(), cols.len() + 1, "timestamp column plus one per sensor");

        // Values line up with their headers.
        assert_eq!(row[1], "21.500");
        // The unread sensor writes an empty field, not 0.000.
        assert_eq!(row[2], "", "unread sensor must be blank, not zero");
        assert_eq!(row[3], "5.000");
    }

    /// A sensor name containing a comma would otherwise split the header.
    #[test]
    fn csv_fields_with_commas_are_quoted() {
        let cols = column_ids(&frame(), None);
        let header = csv_header(&frame(), &cols);
        assert!(header.contains("\"Apple M5 Comma, Name [%]\""), "{header}");
    }

    #[test]
    fn ndjson_is_one_line_and_parses() {
        let line = ndjson_line(&frame(), None);
        assert!(!line.contains('\n'), "NDJSON records must be single-line");
        let v: serde_json::Value = serde_json::from_str(&line).expect("valid JSON");
        assert_eq!(v["seq"], 3);
    }

    #[test]
    fn filtered_ndjson_is_a_compact_record() {
        let line = ndjson_line(&frame(), Some("package"));
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let sensors = v["sensors"].as_array().unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0]["name"], "CPU Package Power");
        assert_eq!(sensors[0]["unit"], "W");
        assert_eq!(v["unix_ms"], 1_738_000_000_123u64);
    }

    #[test]
    fn filter_selects_columns() {
        let cols = column_ids(&frame(), Some("package"));
        assert_eq!(cols, vec!["/p".to_string()]);
    }
}
