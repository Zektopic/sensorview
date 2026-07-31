//! The shared telemetry store — the one synchronization primitive the whole
//! app is built around.
//!
//! # The problem it solves
//!
//! Three threads need the latest hardware state: the poller writes it, the GUI
//! renders it at up to 60 Hz, and the web server fans it out to every connected
//! browser. The obvious `Arc<Mutex<State>>` makes those three contend on one
//! lock, and the previous design compounded it by deep-cloning the whole
//! hardware tree on every UI frame, in every window.
//!
//! # The design
//!
//! One writer, lock-free readers, and exactly one serialization per tick:
//!
//! - The poll thread builds an immutable [`TelemetryFrame`] and calls
//!   [`TelemetryStore::publish`].
//! - `publish` swaps an atomic pointer ([`ArcSwap`]), so [`TelemetryStore::load`]
//!   is a pointer read that never blocks and can never be poisoned.
//! - `publish` also serializes the frame to JSON **once** and hands the same
//!   `Arc<String>` to every WebSocket client, so N clients cost O(1), not O(N).
//!
//! # Why this cannot deadlock
//!
//! 1. **Single writer.** Only the poll thread mutates; nothing else needs a
//!    write path, so there is no writer-writer interaction at all.
//! 2. **Readers never block.** `ArcSwap::load_full` is an atomic refcount bump.
//! 3. **No guard is ever held across `.await`.** The async tasks touch only the
//!    `ArcSwap` and the broadcast channel, neither of which yields a guard.
//!    `web/` denies `clippy::await_holding_lock` to keep it that way.
//! 4. **The one real lock is a leaf.** [`history`](TelemetryStore::history) uses
//!    an `RwLock`, but it is never acquired while holding any other lock and
//!    never held across an await, so no lock-ordering cycle can form.
//! 5. **Backpressure stops at the channel.** `broadcast::Sender::send` is
//!    synchronous and non-blocking; a slow browser gets `Lagged` and resyncs. A
//!    stalled client can never slow the hardware loop or the UI.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::ArcSwap;
use serde::Serialize;

use crate::inventory::Inventory;
use crate::model::Hardware;
use crate::source::Diagnostics;

/// Samples retained per sensor for the graph panes and `/api/history`.
/// At the default 1 Hz fast lane this is ~10 minutes.
pub const HISTORY_LEN: usize = 600;

/// One complete, immutable observation of the machine.
///
/// Cheap to share (`Arc`) and never mutated after publication, which is what
/// makes lock-free reads sound.
#[derive(Debug, Clone, Serialize)]
pub struct TelemetryFrame {
    /// Monotonic tick counter. Gaps in what a client receives mean it lagged.
    pub seq: u64,
    pub unix_ms: u64,
    /// Fast lane: the enriched sensor tree (min/max/avg already folded in).
    pub tree: Vec<Hardware>,
    /// Slow lane: storage health, PCIe topology, hex blobs.
    pub inventory: Arc<Inventory>,
    /// Sensor-engine and kernel-driver status.
    pub diagnostics: Diagnostics,
    /// Per-drive S.M.A.R.T. health from the *sensor* backend.
    ///
    /// Carried beside `inventory` rather than merged into it because the two
    /// arrive from different places: macOS fills `inventory.storage` from its
    /// own slow-lane backend, while on Windows the health comes from the sensor
    /// sidecar, which the inventory backend cannot reach. Merging would mean
    /// rebuilding the `Arc<Inventory>` — and cloning its hex blobs — every
    /// tick. Read both through [`TelemetryFrame::storage`].
    pub sensor_storage: Vec<crate::model::storage::StorageHealth>,
    /// Name of the active sensor backend.
    pub source: String,
}

impl Default for TelemetryFrame {
    fn default() -> Self {
        Self {
            seq: 0,
            unix_ms: 0,
            tree: Vec::new(),
            inventory: Arc::new(Inventory::default()),
            diagnostics: Diagnostics::default(),
            sensor_storage: Vec::new(),
            source: String::new(),
        }
    }
}

impl TelemetryFrame {
    /// Drive health, from whichever lane produced it on this platform.
    ///
    /// One accessor so callers never have to know that macOS fills the slow
    /// lane and Windows the sensor backend.
    pub fn storage(&self) -> &[crate::model::storage::StorageHealth] {
        if self.sensor_storage.is_empty() {
            &self.inventory.storage
        } else {
            &self.sensor_storage
        }
    }

    /// Total sensor count across the tree, including sub-hardware.
    pub fn sensor_count(&self) -> usize {
        fn walk(hw: &[Hardware]) -> usize {
            hw.iter().map(|h| h.sensors.len() + walk(&h.sub_hardware)).sum()
        }
        walk(&self.tree)
    }

    /// Look up a sensor anywhere in the tree by identifier.
    // Consumed by the GUI graph window and the web history endpoint; a
    // CLI-only build has neither.
    #[allow(dead_code)]
    pub fn find_sensor(&self, identifier: &str) -> Option<&crate::model::Sensor> {
        fn walk<'a>(hw: &'a [Hardware], id: &str) -> Option<&'a crate::model::Sensor> {
            for h in hw {
                if let Some(s) = h.sensors.iter().find(|s| s.identifier == id) {
                    return Some(s);
                }
                if let Some(s) = walk(&h.sub_hardware, id) {
                    return Some(s);
                }
            }
            None
        }
        walk(&self.tree, identifier)
    }
}

/// Publish/subscribe hub for telemetry. Construct once, share via `Arc`.
pub struct TelemetryStore {
    frame: ArcSwap<TelemetryFrame>,
    /// The current frame, pre-serialized. Shared by every reader so the cost is
    /// paid once per tick regardless of how many clients are connected.
    /// Exists only for the web tier — without it there is nothing to serialize
    /// *for*, so both the field and the work disappear with the feature.
    #[cfg(feature = "web")]
    json: ArcSwap<String>,
    /// Per-sensor sample rings. Leaf lock — see the deadlock argument above.
    history: RwLock<HashMap<String, VecDeque<f32>>>,
    #[cfg(feature = "web")]
    tx: tokio::sync::broadcast::Sender<Arc<String>>,
}

impl TelemetryStore {
    /// `channel_capacity` is how many ticks a WebSocket client may fall behind
    /// before it is told it lagged and resyncs from the latest frame.
    pub fn new(channel_capacity: usize) -> Self {
        #[cfg(not(feature = "web"))]
        let _ = channel_capacity; // no broadcast channel without the web tier
        Self {
            frame: ArcSwap::from_pointee(TelemetryFrame::default()),
            #[cfg(feature = "web")]
            json: ArcSwap::from_pointee(String::from("{}")),
            history: RwLock::new(HashMap::new()),
            #[cfg(feature = "web")]
            tx: tokio::sync::broadcast::channel(channel_capacity).0,
        }
    }

    /// Publish a new frame. **Poll thread only** — this is the single writer.
    ///
    /// Records history, serializes once, swaps both pointers, then broadcasts.
    /// Never blocks: a full broadcast channel drops the oldest value rather than
    /// applying backpressure to the caller.
    pub fn publish(&self, frame: TelemetryFrame) {
        self.record_history(&frame.tree);

        #[cfg(feature = "web")]
        {
            let json = Arc::new(serde_json::to_string(&frame).unwrap_or_else(|e| {
                // Serialization of our own owned types should not fail; if it
                // ever does, keep the app running and make the failure visible.
                format!(r#"{{"error":"telemetry serialization failed: {e}"}}"#)
            }));
            self.json.store(json.clone());
            // Err means "no subscribers", which is the normal case with no
            // browser attached. Never an error condition for the poller.
            let _ = self.tx.send(json);
        }

        self.frame.store(Arc::new(frame));
    }

    /// The latest frame. Lock-free; safe to call every UI frame.
    pub fn load(&self) -> Arc<TelemetryFrame> {
        self.frame.load_full()
    }

    /// The latest frame, pre-serialized. Shared allocation — do not mutate.
    #[cfg(feature = "web")]
    pub fn json(&self) -> Arc<String> {
        self.json.load_full()
    }

    /// Subscribe to the per-tick broadcast. Each WebSocket client owns one.
    #[cfg(feature = "web")]
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Arc<String>> {
        self.tx.subscribe()
    }

    /// Number of live WebSocket subscribers (shown in the UI status bar).
    #[cfg(feature = "web")]
    // Shown in the GUI's status bar; no other front end displays it.
    #[allow(dead_code)]
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Samples for one sensor, oldest → newest.
    // As `find_sensor`: GUI graphs and /api/history.
    #[allow(dead_code)]
    pub fn history(&self, identifier: &str) -> Vec<f32> {
        self.history
            .read()
            .ok()
            .and_then(|h| h.get(identifier).map(|q| q.iter().copied().collect()))
            .unwrap_or_default()
    }

    /// Drop all retained samples (paired with the UI's "reset min/max").
    pub fn clear_history(&self) {
        if let Ok(mut h) = self.history.write() {
            h.clear();
        }
    }

    /// Fold this tick's readings into the per-sensor rings.
    fn record_history(&self, tree: &[Hardware]) {
        let Ok(mut hist) = self.history.write() else { return };
        fn walk(hw: &[Hardware], hist: &mut HashMap<String, VecDeque<f32>>) {
            for h in hw {
                for s in &h.sensors {
                    let Some(v) = s.value else { continue };
                    let ring = hist
                        .entry(s.identifier.clone())
                        .or_insert_with(|| VecDeque::with_capacity(HISTORY_LEN));
                    if ring.len() == HISTORY_LEN {
                        ring.pop_front();
                    }
                    ring.push_back(v);
                }
                walk(&h.sub_hardware, hist);
            }
        }
        walk(tree, &mut hist);
    }
}

/// Current wall-clock time in milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HardwareType, Sensor, SensorType};

    fn tree_with(id: &str, value: f32) -> Vec<Hardware> {
        vec![Hardware {
            identifier: "/cpu/0".into(),
            name: "CPU".into(),
            hardware_type: HardwareType::Cpu,
            sensors: vec![Sensor {
                identifier: id.into(),
                name: "Core #1".into(),
                sensor_type: SensorType::Temperature,
                index: 0,
                value: Some(value),
                min: None,
                max: None,
                avg: None,
            }],
            sub_hardware: vec![],
        }]
    }

    fn frame(seq: u64, value: f32) -> TelemetryFrame {
        TelemetryFrame {
            seq,
            unix_ms: now_ms(),
            tree: tree_with("/cpu/0/temperature/0", value),
            source: "test".into(),
            ..Default::default()
        }
    }

    #[test]
    fn publish_then_load_returns_the_new_frame() {
        let store = TelemetryStore::new(16);
        assert_eq!(store.load().seq, 0);

        store.publish(frame(1, 42.0));
        let f = store.load();
        assert_eq!(f.seq, 1);
        assert_eq!(f.sensor_count(), 1);
        assert_eq!(f.find_sensor("/cpu/0/temperature/0").unwrap().value, Some(42.0));
        assert!(f.find_sensor("/nope").is_none());
    }

    #[cfg(feature = "web")]
    #[test]
    fn json_is_serialized_once_and_shared() {
        let store = TelemetryStore::new(16);
        store.publish(frame(1, 42.0));

        let a = store.json();
        let b = store.json();
        // Both readers get the same allocation — the point of the design.
        assert!(Arc::ptr_eq(&a, &b));
        assert!(a.contains(r#""seq":1"#));
        assert!(a.contains("42.0"));
    }

    #[test]
    fn history_accumulates_and_is_bounded() {
        let store = TelemetryStore::new(16);
        for i in 0..3 {
            store.publish(frame(i, 10.0 + i as f32));
        }
        assert_eq!(store.history("/cpu/0/temperature/0"), vec![10.0, 11.0, 12.0]);
        assert!(store.history("/unknown").is_empty());

        for i in 0..HISTORY_LEN + 50 {
            store.publish(frame(i as u64, i as f32));
        }
        let h = store.history("/cpu/0/temperature/0");
        assert_eq!(h.len(), HISTORY_LEN, "ring must stay bounded");
        // Oldest samples were evicted; newest is the last published value.
        assert_eq!(*h.last().unwrap(), (HISTORY_LEN + 49) as f32);

        store.clear_history();
        assert!(store.history("/cpu/0/temperature/0").is_empty());
    }

    #[cfg(feature = "web")]
    #[test]
    fn subscribers_receive_each_published_frame() {
        use tokio::sync::broadcast::error::TryRecvError;

        let store = TelemetryStore::new(16);
        // Publishing with no subscribers must not error or block.
        store.publish(frame(1, 1.0));
        assert_eq!(store.subscriber_count(), 0);

        let mut rx = store.subscribe();
        assert_eq!(store.subscriber_count(), 1);
        // A late subscriber gets the *next* frame, not the one already sent.
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

        store.publish(frame(2, 2.0));
        let msg = rx.try_recv().expect("subscriber receives the new frame");
        assert!(msg.contains(r#""seq":2"#));
    }

    #[cfg(feature = "web")]
    #[test]
    fn slow_subscriber_lags_instead_of_blocking_the_publisher() {
        use tokio::sync::broadcast::error::TryRecvError;

        let store = TelemetryStore::new(4);
        let mut rx = store.subscribe();

        // Publish well past the channel capacity without ever reading. The
        // publisher (the poll thread) must not block or fail.
        for i in 1..=20 {
            store.publish(frame(i, i as f32));
        }

        // The slow client is told it lagged rather than silently seeing a gap.
        match rx.try_recv() {
            Err(TryRecvError::Lagged(n)) => assert!(n > 0, "lagged count reported"),
            other => panic!("expected Lagged, got {other:?}"),
        }
        // And it can resync from the store's latest frame.
        assert_eq!(store.load().seq, 20);
        assert!(store.json().contains(r#""seq":20"#));
    }
}
