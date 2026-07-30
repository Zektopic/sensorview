//! Pushing telemetry to an external sink.
//!
//! The counterpart to `stream`: instead of writing to stdout for something
//! else to consume, the daemon actively posts to a collector on an interval.
//!
//! Mirrors the [`crate::source::SensorSource`] shape — a [`PushSink`] trait
//! with interchangeable implementations — so MQTT or a different time-series
//! database slots in without touching the call site.
//!
//! Runs on its own thread reading `store.load()`. A sink that is slow, down or
//! wedged must never stall the poller, so nothing here is on the hardware path
//! and failures back off rather than retrying tightly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use crate::model::{Hardware, Sensor};
use crate::state::{TelemetryFrame, TelemetryStore};

/// Longest gap between retries after repeated failures.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// A destination for telemetry.
pub trait PushSink: Send {
    fn name(&self) -> &'static str;
    /// Deliver one frame. Returning `Err` triggers backoff; the frame is
    /// dropped rather than queued — for live telemetry the newest sample is
    /// the only interesting one, and an unbounded queue would be a memory leak
    /// pointed at a collector that is already down.
    fn send(&mut self, frame: &TelemetryFrame) -> Result<(), String>;
}

/// Redact anything secret in a push URL before it is printed.
///
/// Push targets routinely carry credentials — `http://user:pass@host` userinfo,
/// and InfluxDB 1.x takes `?u=&p=` in the query string. The daemon prints its
/// configured sinks at startup, and that output lands in terminals, systemd
/// journals and CI logs, so it must not carry the password.
pub fn redact_url(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s, r),
        None => ("", url),
    };

    // Strip userinfo: everything before the last '@' in the authority.
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let authority = match authority.rsplit_once('@') {
        Some((_userinfo, host)) => format!("***@{host}"),
        None => authority.to_string(),
    };

    // Blank the value of any query parameter that looks like a credential.
    let path = match path.split_once('?') {
        None => path.to_string(),
        Some((base, query)) => {
            let scrubbed: Vec<String> = query
                .split('&')
                .map(|pair| {
                    let (key, _) = pair.split_once('=').unwrap_or((pair, ""));
                    let secret = matches!(
                        key.to_ascii_lowercase().as_str(),
                        "p" | "password" | "pass" | "token" | "secret" | "apikey" | "api_key" | "key"
                    );
                    if secret {
                        format!("{key}=***")
                    } else {
                        pair.to_string()
                    }
                })
                .collect();
            format!("{base}?{}", scrubbed.join("&"))
        }
    };

    if scheme.is_empty() {
        format!("{authority}{path}")
    } else {
        format!("{scheme}://{authority}{path}")
    }
}

/// True when the target sends telemetry in cleartext to somewhere other than
/// this machine. Telemetry includes hardware serials, so that deserves a
/// warning even though it is the user's explicit choice.
pub fn is_cleartext_offhost(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "http" | "influx" | "influxdb") {
        return false;
    }
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // Strip the port; keep it simple and treat bracketed IPv6 literally.
    let host = host.rsplit_once(':').map_or(host, |(h, p)| {
        if p.chars().all(|c| c.is_ascii_digit()) { h } else { host }
    });
    !matches!(host.trim_matches(['[', ']']), "localhost" | "127.0.0.1" | "::1")
}

/// Build a sink from a URL. `influx://` and `influxdb://` speak line protocol;
/// `http://` and `https://` post the frame as JSON.
pub fn sink_from_url(url: &str) -> Result<Box<dyn PushSink>, String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("push target {:?} has no scheme (try http:// or influx://)", redact_url(url)))?;
    match scheme {
        "influx" | "influxdb" => Ok(Box::new(InfluxSink::new(format!("http://{rest}")))),
        "influxs" => Ok(Box::new(InfluxSink::new(format!("https://{rest}")))),
        "http" | "https" => Ok(Box::new(WebhookSink::new(url.to_string()))),
        other => Err(format!(
            "unsupported push scheme {other:?} — expected http, https, influx or influxs"
        )),
    }
}

/// Spawn the push loop. Dropping the handle stops it.
pub struct PushHandle {
    running: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl PushHandle {
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for PushHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn spawn(
    store: Arc<TelemetryStore>,
    mut sink: Box<dyn PushSink>,
    interval: Duration,
) -> PushHandle {
    let running = Arc::new(AtomicBool::new(true));
    let flag = running.clone();

    let join = std::thread::Builder::new()
        .name("sensorview-push".into())
        .spawn(move || {
            let mut backoff = Duration::ZERO;
            let mut last_seq = 0u64;
            let mut failures = 0u32;

            while flag.load(Ordering::Relaxed) {
                sleep_interruptible(&flag, interval + backoff);
                if !flag.load(Ordering::Relaxed) {
                    break;
                }

                let frame = store.load();
                // Nothing new since the last push: don't send a duplicate.
                if frame.seq == last_seq || frame.seq == 0 {
                    continue;
                }

                match sink.send(&frame) {
                    Ok(()) => {
                        last_seq = frame.seq;
                        if failures > 0 {
                            eprintln!("sensorview: {} recovered", sink.name());
                        }
                        failures = 0;
                        backoff = Duration::ZERO;
                    }
                    Err(e) => {
                        failures += 1;
                        // Log the first failure and then every tenth, so a
                        // collector that is down overnight doesn't fill the log.
                        if failures == 1 || failures.is_multiple_of(10) {
                            eprintln!("sensorview: {} failed ({failures}): {e}", sink.name());
                        }
                        backoff = (backoff * 2 + Duration::from_secs(1)).min(MAX_BACKOFF);
                    }
                }
            }
        })
        .expect("spawn push thread");

    PushHandle { running, join: Some(join) }
}

/// Sleep in slices so shutdown doesn't wait out a full backoff.
fn sleep_interruptible(running: &AtomicBool, total: Duration) {
    let mut left = total;
    let slice = Duration::from_millis(100);
    while left > Duration::ZERO && running.load(Ordering::Relaxed) {
        let step = slice.min(left);
        std::thread::sleep(step);
        left -= step;
    }
}

// ---- InfluxDB line protocol ----------------------------------------------

pub struct InfluxSink {
    url: String,
}

impl InfluxSink {
    pub fn new(url: String) -> Self {
        Self { url }
    }
}

impl PushSink for InfluxSink {
    fn name(&self) -> &'static str {
        "influxdb"
    }

    fn send(&mut self, frame: &TelemetryFrame) -> Result<(), String> {
        let body = line_protocol(frame);
        if body.is_empty() {
            return Ok(());
        }
        let response = ureq::post(&self.url)
            .header("Content-Type", "text/plain; charset=utf-8")
            .send(&body)
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(format!("HTTP {status}"))
        }
    }
}

/// One InfluxDB line-protocol point per sensor, nanosecond timestamps.
///
/// `measurement,tag=value field=number timestamp`
pub fn line_protocol(frame: &TelemetryFrame) -> String {
    let ns = frame.unix_ms.saturating_mul(1_000_000);
    let mut out = String::new();
    walk(&frame.tree, &mut |hw, s| {
        let Some(v) = s.value else {
            // A sensor with no reading writes no point — a gap is the truth,
            // and Influx would otherwise record a fabricated value.
            return;
        };
        if !v.is_finite() {
            return;
        }
        out.push_str(&format!(
            "sensorview,hardware={},sensor={},type={:?} value={} {}\n",
            escape_tag(hw),
            escape_tag(&s.name),
            s.sensor_type,
            v,
            ns
        ));
    });
    out
}

/// Line protocol needs commas, spaces and equals signs escaped in tag values.
fn escape_tag(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace(',', "\\,")
        .replace(' ', "\\ ")
        .replace('=', "\\=")
}

// ---- Generic JSON webhook -------------------------------------------------

pub struct WebhookSink {
    url: String,
}

impl WebhookSink {
    pub fn new(url: String) -> Self {
        Self { url }
    }
}

impl PushSink for WebhookSink {
    fn name(&self) -> &'static str {
        "webhook"
    }

    fn send(&mut self, frame: &TelemetryFrame) -> Result<(), String> {
        let body = serde_json::to_string(frame).map_err(|e| e.to_string())?;
        let response = ureq::post(&self.url)
            .header("Content-Type", "application/json")
            .send(&body)
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(format!("HTTP {status}"))
        }
    }
}

fn walk<'a>(tree: &'a [Hardware], f: &mut impl FnMut(&'a str, &'a Sensor)) {
    for hw in tree {
        for s in &hw.sensors {
            f(&hw.name, s);
        }
        walk(&hw.sub_hardware, f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HardwareType, SensorType};

    fn frame() -> TelemetryFrame {
        TelemetryFrame {
            seq: 4,
            unix_ms: 1_738_000_000_123,
            tree: vec![Hardware {
                identifier: "/cpu/0".into(),
                name: "Apple M5".into(),
                hardware_type: HardwareType::Cpu,
                sensors: vec![
                    Sensor {
                        identifier: "/p".into(),
                        name: "CPU Package Power".into(),
                        sensor_type: SensorType::Power,
                        index: 0,
                        value: Some(21.5),
                        min: None,
                        max: None,
                        avg: None,
                    },
                    Sensor {
                        identifier: "/q".into(),
                        name: "Quiet".into(),
                        sensor_type: SensorType::Temperature,
                        index: 1,
                        value: None,
                        min: None,
                        max: None,
                        avg: None,
                    },
                ],
                sub_hardware: Vec::new(),
            }],
            ..Default::default()
        }
    }

    /// The daemon prints its sinks at startup and that output reaches
    /// journals and CI logs, so a password in the URL must never survive.
    #[test]
    fn redaction_removes_userinfo_and_secret_query_params() {
        assert_eq!(
            redact_url("http://alice:hunter2@collector:8086/write?db=sensors"),
            "http://***@collector:8086/write?db=sensors"
        );
        // InfluxDB 1.x credentials ride in the query string.
        assert_eq!(
            redact_url("influx://h:8086/write?db=s&u=admin&p=hunter2"),
            "influx://h:8086/write?db=s&u=admin&p=***"
        );
        for secret in ["token", "password", "apikey", "api_key", "secret", "key"] {
            let out = redact_url(&format!("https://h/i?{secret}=hunter2"));
            assert!(!out.contains("hunter2"), "{secret} leaked: {out}");
        }
        // Nothing sensitive: left exactly as it was.
        assert_eq!(
            redact_url("https://collector/ingest?db=sensors"),
            "https://collector/ingest?db=sensors"
        );
    }

    /// A parse failure must not echo the raw URL back either.
    #[test]
    fn error_messages_are_redacted_too() {
        let Err(err) = sink_from_url("alice:hunter2@host/path") else {
            panic!("a schemeless URL must be rejected");
        };
        assert!(!err.contains("hunter2"), "credentials leaked in error: {err}");
    }

    #[test]
    fn cleartext_warning_only_fires_for_remote_plaintext() {
        assert!(is_cleartext_offhost("http://collector/ingest"));
        assert!(is_cleartext_offhost("influx://10.0.0.5:8086/write"));
        // Local is fine — nothing leaves the machine.
        assert!(!is_cleartext_offhost("http://127.0.0.1:8086/write"));
        assert!(!is_cleartext_offhost("http://localhost:8086/write"));
        assert!(!is_cleartext_offhost("influx://[::1]:8086/write"));
        // Encrypted is fine wherever it goes.
        assert!(!is_cleartext_offhost("https://collector/ingest"));
        assert!(!is_cleartext_offhost("influxs://collector:8086/write"));
    }

    #[test]
    fn scheme_selects_the_sink() {
        assert_eq!(sink_from_url("influx://h:8086/write?db=x").unwrap().name(), "influxdb");
        assert_eq!(sink_from_url("http://h/ingest").unwrap().name(), "webhook");
        assert_eq!(sink_from_url("https://h/ingest").unwrap().name(), "webhook");
        assert!(sink_from_url("mqtt://broker/topic").is_err(), "unsupported scheme must be rejected");
        assert!(sink_from_url("no-scheme").is_err());
    }

    /// Spaces in names would otherwise split a line-protocol point into
    /// measurement and field sections at the wrong place.
    #[test]
    fn line_protocol_escapes_tag_values() {
        let out = line_protocol(&frame());
        assert!(out.contains("hardware=Apple\\ M5"), "{out}");
        assert!(out.contains("sensor=CPU\\ Package\\ Power"), "{out}");
    }

    #[test]
    fn line_protocol_uses_nanoseconds_and_one_line_per_sensor() {
        let out = line_protocol(&frame());
        let lines: Vec<&str> = out.lines().collect();
        // Only the sensor that has a reading.
        assert_eq!(lines.len(), 1, "{out}");
        assert!(lines[0].ends_with(" 1738000000123000000"), "ns timestamp: {}", lines[0]);
        assert!(lines[0].contains("value=21.5"), "{}", lines[0]);
    }

    /// A gap must stay a gap: writing 0 for an unread sensor would show up in
    /// Grafana as a real dip to zero.
    #[test]
    fn unread_sensors_produce_no_point() {
        let out = line_protocol(&frame());
        assert!(!out.contains("Quiet"), "unread sensor must not be written: {out}");
    }

    #[test]
    fn empty_tree_produces_no_body() {
        let empty = TelemetryFrame { seq: 1, ..Default::default() };
        assert!(line_protocol(&empty).is_empty());
    }
}
