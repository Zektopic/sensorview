//! **Thread 3 — the embedded web server.**
//!
//! Serves a bundled single-page dashboard and streams live telemetry over
//! `/ws/telemetry` to browsers on the LAN. Runs a small tokio runtime on its
//! own thread; nothing else in the app is async.
//!
//! # Security posture
//!
//! This endpoint exposes Ring-0-derived telemetry, drive serial numbers, and
//! raw SPD / PCI-config hex dumps. It therefore binds **loopback by default**.
//! Exposing it to the LAN is opt-in, and when the bind address is not loopback
//! a token is *required* — generated at startup and shown in the UI, never
//! persisted. See [`auth`].
//!
//! # Concurrency contract
//!
//! Handlers read telemetry only through [`TelemetryStore::load`] /
//! [`TelemetryStore::json`] (lock-free atomic pointer reads) and the broadcast
//! channel. No `Mutex`/`RwLock` guard is ever held across an `.await` — the
//! lint below makes that a compile error rather than a code-review convention.

#![deny(clippy::await_holding_lock)]

pub mod api;
pub mod assets;
pub mod auth;
pub mod ws;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::mpsc::sync_channel;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use axum::routing::get;
use axum::Router;
use tokio::sync::watch;

use crate::state::TelemetryStore;
use crate::sysinfo::SystemInfoHandle;

/// How the server should be exposed.
#[derive(Debug, Clone)]
pub struct WebConfig {
    pub enabled: bool,
    pub bind: IpAddr,
    pub port: u16,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Loopback: telemetry is not published to the network unless the
            // user deliberately changes this.
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 8080,
        }
    }
}

impl WebConfig {
    /// True when the configured bind address is reachable from other machines.
    pub fn is_lan_exposed(&self) -> bool {
        !self.bind.is_loopback()
    }

    /// Apply the `SENSORVIEW_WEB_PORT` override, if set and valid.
    ///
    /// 8080 is a crowded port — torrent clients, dev servers and admin UIs all
    /// want it — so there needs to be a way to move without editing
    /// `settings.json`. Port 0 asks the OS for any free port.
    pub fn with_env_overrides(mut self) -> Self {
        if let Ok(v) = std::env::var("SENSORVIEW_WEB_PORT") {
            match v.parse::<u16>() {
                Ok(p) => self.port = p,
                Err(_) => eprintln!("SENSORVIEW_WEB_PORT={v:?} is not a port number; ignoring"),
            }
        }
        // Binding a specific interface, e.g. only the wired NIC. Any
        // non-loopback value turns on the mandatory access token, exactly as
        // the settings toggle does — there is no way to expose the dashboard
        // unauthenticated.
        if let Ok(v) = std::env::var("SENSORVIEW_WEB_BIND") {
            match v.parse::<IpAddr>() {
                Ok(a) => self.bind = a,
                Err(_) => eprintln!("SENSORVIEW_WEB_BIND={v:?} is not an IP address; ignoring"),
            }
        }
        self
    }
}

/// Shared state handed to every axum handler. Cheap to clone.
#[derive(Clone)]
pub struct AppState {
    pub store: Arc<TelemetryStore>,
    /// Static system inventory, filled in by a background WMI/IOKit query.
    pub sysinfo: SystemInfoHandle,
    /// `Some` when a token is required — i.e. when bound off-loopback.
    pub token: Option<Arc<str>>,
    pub started: Instant,
}

/// Handle to the running server thread.
pub struct WebHandle {
    join: Option<JoinHandle<()>>,
    shutdown: watch::Sender<bool>,
    /// The address actually bound, once the listener came up.
    pub bound: Option<SocketAddr>,
    /// Access token, when one is required. Displayed in the UI so the user can
    /// build the dashboard URL.
    pub token: Option<String>,
    /// Why the server isn't running, if it isn't.
    pub error: Option<String>,
}

impl WebHandle {
    /// A handle representing "no server" (disabled, or failed to bind).
    pub fn disabled(error: Option<String>) -> Self {
        Self { join: None, shutdown: watch::channel(true).0, bound: None, token: None, error }
    }

    /// The URL to open, including the token when one is required.
    pub fn url(&self) -> Option<String> {
        let addr = self.bound?;
        // A wildcard bind isn't a dialable host; show loopback for the local
        // link and let the user substitute the machine's LAN address.
        let host = if addr.ip().is_unspecified() {
            format!("127.0.0.1:{}", addr.port())
        } else {
            addr.to_string()
        };
        Some(match &self.token {
            Some(t) => format!("http://{host}/?token={t}"),
            None => format!("http://{host}/"),
        })
    }

    /// Ask the server to finish in-flight requests and stop. Idempotent.
    pub fn stop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
        self.bound = None;
    }
}

impl Drop for WebHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// How long to wait for the listener to bind before giving up and reporting the
/// failure to the UI. Binding is near-instant; this only bounds pathological
/// cases so startup can never hang.
const BIND_TIMEOUT: Duration = Duration::from_secs(5);

/// Spawn Thread 3.
///
/// Returns once the listener has bound (or failed), so the UI can show the real
/// URL — or a real error, such as the port already being in use — instead of a
/// hopeful guess.
pub fn spawn(
    store: Arc<TelemetryStore>,
    sysinfo: SystemInfoHandle,
    config: WebConfig,
) -> WebHandle {
    if !config.enabled {
        return WebHandle::disabled(None);
    }

    // A token is only meaningful when the socket is reachable off-machine.
    let token: Option<Arc<str>> = config.is_lan_exposed().then(|| auth::generate_token().into());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    // Report the bind result back to this thread synchronously.
    let (ready_tx, ready_rx) = sync_channel::<Result<SocketAddr, String>>(1);

    let state = AppState { store, sysinfo, token: token.clone(), started: Instant::now() };
    let addr = SocketAddr::new(config.bind, config.port);

    let join = std::thread::Builder::new()
        .name("web-server".into())
        .spawn(move || {
            // A small runtime: this thread serves telemetry, it does not need
            // to scale to a general-purpose workload.
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("web-worker")
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("tokio runtime: {e}")));
                    return;
                }
            };
            rt.block_on(serve(addr, state, ready_tx, shutdown_rx));
        })
        .expect("spawning the web thread");

    match ready_rx.recv_timeout(BIND_TIMEOUT) {
        Ok(Ok(bound)) => WebHandle {
            join: Some(join),
            shutdown: shutdown_tx,
            bound: Some(bound),
            token: token.map(|t| t.to_string()),
            error: None,
        },
        Ok(Err(e)) => {
            let _ = join.join();
            WebHandle::disabled(Some(e))
        }
        Err(_) => {
            let _ = shutdown_tx.send(true);
            WebHandle::disabled(Some("web server did not start within 5 s".into()))
        }
    }
}

/// Build the router. Public so tests can exercise it without a socket.
/// Response headers applied to every route.
///
/// The dashboard is entirely self-contained — HTML, CSS and JS are embedded in
/// the binary by `rust-embed` and it loads nothing from the network — so the
/// strictest useful CSP costs nothing and removes injected-script and
/// external-exfiltration as a class.
///
/// `Referrer-Policy` is the load-bearing one. `auth::presented_token` accepts
/// `?token=` in the query string (a browser cannot set an Authorization header
/// when you paste a URL), so without this any navigation away from the
/// dashboard would put the access token in a `Referer` header.
/// Names must be lowercase: `HeaderName::from_static` panics on anything else,
/// and this runs on the web thread where `panic = "abort"` would take the whole
/// process down. `security_header_names_are_valid` guards it.
const SECURITY_HEADERS: &[(&str, &str)] = &[
    (
        "content-security-policy",
        concat!(
            "default-src 'none'; ",
            "script-src 'self'; ",
            // egui's dashboard styles inline; scripts are not allowed to.
            "style-src 'self' 'unsafe-inline'; ",
            "img-src 'self' data:; ",
            // Same-origin XHR plus the telemetry WebSocket.
            "connect-src 'self' ws: wss:; ",
            "base-uri 'none'; ",
            "form-action 'none'; ",
            "frame-ancestors 'none'"
        ),
    ),
    ("referrer-policy", "no-referrer"),
    ("x-content-type-options", "nosniff"),
    // Redundant with frame-ancestors above, for older browsers.
    ("x-frame-options", "DENY"),
    // Telemetry is not something a cache or proxy should retain. Applied only
    // where a handler hasn't already chosen a policy — `assets` sets
    // `no-cache` for the embedded dashboard, which is deliberate.
    ("cache-control", "no-store"),
];

pub fn router(state: AppState) -> Router {
    use tower_http::compression::CompressionLayer;
    use tower_http::set_header::SetResponseHeaderLayer;

    let public = Router::new()
        // Liveness is intentionally unauthenticated so a monitoring probe
        // doesn't need the token. It exposes no telemetry.
        .route("/api/health", get(api::health));

    let protected = Router::new()
        .route("/", get(assets::index))
        .route("/{*path}", get(assets::static_file))
        .route("/api/telemetry", get(api::telemetry))
        .route("/api/system", get(api::system))
        .route("/api/history/{identifier}", get(api::history))
        .route("/metrics", get(api::metrics))
        .route("/ws/telemetry", get(ws::handler))
        .layer(axum::middleware::from_fn_with_state(state.clone(), auth::require_token));

    let mut app = public
        .merge(protected)
        // gzip: the telemetry JSON is highly repetitive and compresses ~10:1,
        // which matters over Wi-Fi at 1 Hz.
        .layer(CompressionLayer::new());

    for (name, value) in SECURITY_HEADERS {
        // `if_not_present` so a handler that deliberately sets its own value
        // (Content-Type, say) is never overridden.
        app = app.layer(SetResponseHeaderLayer::if_not_present(
            axum::http::HeaderName::from_static(name),
            axum::http::HeaderValue::from_static(value),
        ));
    }

    app.with_state(state)
}

async fn serve(
    addr: SocketAddr,
    state: AppState,
    ready: std::sync::mpsc::SyncSender<Result<SocketAddr, String>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            let _ = ready.send(Err(format!("cannot bind {addr}: {e}")));
            return;
        }
    };
    let bound = listener.local_addr().unwrap_or(addr);
    let _ = ready.send(Ok(bound));

    let app = router(state);
    let _ = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            // Resolves when main flips the flag on exit.
            let _ = shutdown.wait_for(|stop| *stop).await;
        })
        .await;
}

#[cfg(test)]
mod tests {
    /// `HeaderName::from_static` panics unless the name is lowercase and
    /// otherwise valid, and `router()` builds these on the web thread — where
    /// `panic = "abort"` would take the process with it. Capitalised names got
    /// this exact panic once already.
    #[test]
    fn security_header_names_and_values_are_valid() {
        for (name, value) in super::SECURITY_HEADERS {
            assert_eq!(
                *name,
                name.to_ascii_lowercase(),
                "header name {name:?} must be lowercase for HeaderName::from_static"
            );
            // These are the exact constructors `router()` uses.
            let _ = axum::http::HeaderName::from_static(name);
            let _ = axum::http::HeaderValue::from_static(value);
        }
    }

    use super::*;

    #[test]
    fn loopback_default_is_not_lan_exposed() {
        let cfg = WebConfig::default();
        assert!(!cfg.is_lan_exposed());
        assert_eq!(cfg.port, 8080);
    }

    #[test]
    fn wildcard_and_explicit_addresses_are_lan_exposed() {
        for ip in ["0.0.0.0", "192.168.1.10"] {
            let cfg = WebConfig { bind: ip.parse().unwrap(), ..Default::default() };
            assert!(cfg.is_lan_exposed(), "{ip} should count as exposed");
        }
    }

    #[test]
    fn env_overrides_apply_and_cannot_bypass_the_token() {
        // Serialised implicitly: this is the only test touching these vars.
        std::env::set_var("SENSORVIEW_WEB_PORT", "9191");
        std::env::set_var("SENSORVIEW_WEB_BIND", "0.0.0.0");
        let cfg = WebConfig::default().with_env_overrides();
        assert_eq!(cfg.port, 9191);
        assert!(cfg.is_lan_exposed(), "an env-set LAN bind still demands a token");

        // Garbage is ignored rather than crashing or silently binding wide.
        std::env::set_var("SENSORVIEW_WEB_PORT", "not-a-port");
        std::env::set_var("SENSORVIEW_WEB_BIND", "not-an-ip");
        let cfg = WebConfig::default().with_env_overrides();
        assert_eq!(cfg.port, 8080);
        assert!(!cfg.is_lan_exposed());

        std::env::remove_var("SENSORVIEW_WEB_PORT");
        std::env::remove_var("SENSORVIEW_WEB_BIND");
    }

    #[test]
    fn disabled_config_yields_a_handle_that_is_not_running() {
        let store = Arc::new(TelemetryStore::new(4));
        let sysinfo = crate::sysinfo::SystemInfoHandle::default();
        let h = spawn(store, sysinfo, WebConfig { enabled: false, ..Default::default() });
        assert!(h.bound.is_none());
        assert!(h.url().is_none());
        assert!(h.error.is_none());
    }

    #[test]
    fn binds_loopback_without_a_token_and_reports_the_url() {
        let store = Arc::new(TelemetryStore::new(4));
        let sysinfo = crate::sysinfo::SystemInfoHandle::default();
        // Port 0: let the OS pick, so the test can't collide with a real server.
        let mut h = spawn(store, sysinfo, WebConfig { port: 0, ..Default::default() });
        assert!(h.bound.is_some(), "bind failed: {:?}", h.error);
        assert!(h.token.is_none(), "loopback must not demand a token");
        let url = h.url().unwrap();
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        assert!(!url.contains("token"));
        h.stop();
        assert!(h.bound.is_none());
    }

    #[test]
    fn lan_bind_generates_a_token_and_puts_it_in_the_url() {
        let store = Arc::new(TelemetryStore::new(4));
        let sysinfo = crate::sysinfo::SystemInfoHandle::default();
        let mut h = spawn(
            store,
            sysinfo,
            WebConfig { bind: "0.0.0.0".parse().unwrap(), port: 0, ..Default::default() },
        );
        assert!(h.bound.is_some(), "bind failed: {:?}", h.error);
        let token = h.token.clone().expect("LAN exposure must require a token");
        assert!(token.len() >= 32, "token too short to resist guessing: {token}");
        // A wildcard bind is shown as loopback — 0.0.0.0 is not a dialable host.
        let url = h.url().unwrap();
        assert!(url.starts_with("http://127.0.0.1:"), "{url}");
        assert!(url.contains(&format!("token={token}")));
        h.stop();
    }

    #[test]
    fn port_in_use_is_reported_rather_than_hanging() {
        let store = Arc::new(TelemetryStore::new(4));
        let sysinfo = crate::sysinfo::SystemInfoHandle::default();
        let mut first = spawn(store.clone(), sysinfo.clone(), WebConfig { port: 0, ..Default::default() });
        let port = first.bound.unwrap().port();

        let second = spawn(store, sysinfo, WebConfig { port, ..Default::default() });
        assert!(second.bound.is_none());
        let err = second.error.clone().expect("bind conflict surfaces as an error");
        assert!(err.contains("cannot bind"), "{err}");

        first.stop();
    }
}
