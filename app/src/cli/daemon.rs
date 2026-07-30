//! Headless daemon mode — sensors and the web dashboard, no window.
//!
//! Supersedes `SENSORVIEW_HEADLESS`, which parked the main thread forever with
//! no signal handling, so the poller and web server were never stopped: the
//! process just died and relied on the OS to reclaim the port and the sensor
//! driver. On Windows that could leave the LibreHardwareMonitor sidecar holding
//! WinRing0 open.

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::runtime;
use crate::settings::AppSettings;

pub fn run(
    mut settings: AppSettings,
    port: Option<u16>,
    bind: Option<String>,
    log: bool,
    interval: Option<u64>,
) -> ExitCode {
    // Flags override the persisted settings for this run only — a daemon
    // invocation must not rewrite the GUI's saved preferences.
    if let Some(port) = port {
        settings.web_port = port;
    }
    if let Some(bind) = &bind {
        match bind.parse::<std::net::IpAddr>() {
            Ok(addr) => settings.web_lan_access = !addr.is_loopback(),
            Err(_) => {
                eprintln!("sensorview: --bind expects an IP address, got {bind:?}");
                return ExitCode::FAILURE;
            }
        }
    }
    settings.web_enabled = true;

    let mut rt = runtime::start(&settings);

    // Applied as a command rather than through settings so it takes effect on
    // the next tick and is clamped by the engine (poll::MIN_FAST/MAX_FAST).
    if let Some(ms) = interval {
        rt.poller
            .sender()
            .send(crate::poll::Command::SetInterval(Duration::from_millis(ms)))
            .ok();
    }

    println!("SensorView daemon");
    #[cfg(feature = "web")]
    {
        match (rt.web.url(), &rt.web.error) {
            (Some(url), _) => {
                println!("  dashboard  {url}");
                if let Some(token) = &rt.web.token {
                    println!("  token      {token}");
                    println!("             (required: bound off-loopback)");
                }
            }
            (None, Some(err)) => {
                // A daemon whose whole purpose is the dashboard should not
                // pretend it started successfully.
                eprintln!("sensorview: dashboard unavailable: {err}");
                rt.shutdown();
                return ExitCode::FAILURE;
            }
            (None, None) => println!("  dashboard  disabled"),
        }
    }
    #[cfg(not(feature = "web"))]
    println!("  dashboard  not compiled in (built without the `web` feature)");

    // Report the backend once data exists, so the operator can see whether real
    // sensors came up or it fell back to demo data.
    let frame = match rt.wait_for_usable_frame(Duration::from_secs(20)) {
        Ok(frame) => {
            println!("  source     {} ({} sensors)", frame.source, frame.sensor_count());
            Some(frame)
        }
        Err(_) => {
            eprintln!("  source     no data yet — backend may be unavailable");
            None
        }
    };

    // CSV logging is written by the poll thread through the shared slot, so
    // starting it is just filling that slot. The column set is frozen at start,
    // which is why this waits for a usable frame first.
    if log {
        match frame.as_ref().map(|f| crate::logging::CsvLogger::start(&f.tree)) {
            Some(Ok(logger)) => {
                println!("  log        {}", logger.path().display());
                *rt.logger.lock().expect("fresh logger slot") = Some(logger);
            }
            Some(Err(e)) => eprintln!("sensorview: could not start logging: {e}"),
            None => eprintln!("sensorview: not logging — no sensor data to define columns"),
        }
    }
    println!("Ctrl-C to stop.");

    // The handler runs on its own thread and cannot own the handles, so it only
    // sets a flag; the main thread performs the ordered shutdown.
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    if let Err(e) = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst)) {
        eprintln!("sensorview: could not install signal handler: {e}");
        rt.shutdown();
        return ExitCode::FAILURE;
    }

    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
    }

    println!("\nStopping…");
    if let Ok(slot) = rt.logger.lock() {
        if let Some(logger) = slot.as_ref() {
            println!("  wrote {} rows to {}", logger.rows(), logger.path().display());
        }
    }
    let uptime = rt.started.elapsed();
    rt.shutdown();
    println!("Stopped after {}h{:02}m{:02}s.",
        uptime.as_secs() / 3600, (uptime.as_secs() % 3600) / 60, uptime.as_secs() % 60);
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {

    /// `--bind` decides LAN exposure, which in turn decides whether the web
    /// tier demands a token. Getting the loopback test backwards would silently
    /// publish telemetry — hardware serials included — with no auth.
    #[test]
    fn bind_address_determines_lan_exposure() {
        for (addr, lan) in [
            ("127.0.0.1", false),
            ("::1", false),
            ("0.0.0.0", true),
            ("192.168.1.10", true),
        ] {
            let parsed: std::net::IpAddr = addr.parse().unwrap();
            assert_eq!(!parsed.is_loopback(), lan, "{addr} misclassified");
        }
    }
}
