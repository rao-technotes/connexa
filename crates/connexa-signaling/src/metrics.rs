//! Counters exported by the server's `/metrics` endpoint.

use std::sync::atomic::{AtomicU64, Ordering};

use connexa_protocol::ErrorCode;
use dashmap::DashMap;

#[derive(Default)]
pub struct HubMetrics {
    pub connections_total: AtomicU64,
    pub rooms_created_total: AtomicU64,
    pub joins_total: AtomicU64,
    pub resumes_total: AtomicU64,
    pub lobby_requests_total: AtomicU64,
    pub admitted_total: AtomicU64,
    pub denied_total: AtomicU64,
    pub pin_failures_total: AtomicU64,
    pub rate_limited_total: AtomicU64,
    pub relayed_total: AtomicU64,
    pub devices_verified_total: AtomicU64,
    errors: DashMap<String, u64>,
}

/// A point-in-time copy of every counter.
#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    pub counters: Vec<(&'static str, &'static str, u64)>,
    /// Errors returned to clients, by error code.
    pub errors: Vec<(String, u64)>,
}

impl HubMetrics {
    pub fn inc(&self, counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn error(&self, code: ErrorCode) {
        let name = serde_json::to_value(code)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| "unknown".into());
        *self.errors.entry(name).or_default() += 1;
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let mut errors: Vec<(String, u64)> = self
            .errors
            .iter()
            .map(|e| (e.key().clone(), *e.value()))
            .collect();
        errors.sort();
        MetricsSnapshot {
            counters: vec![
                (
                    "connexa_connections_total",
                    "WebSocket connections accepted",
                    get(&self.connections_total),
                ),
                (
                    "connexa_rooms_created_total",
                    "Rooms created",
                    get(&self.rooms_created_total),
                ),
                (
                    "connexa_joins_total",
                    "Participants that joined a room",
                    get(&self.joins_total),
                ),
                (
                    "connexa_resumes_total",
                    "Sessions resumed after a dropped connection",
                    get(&self.resumes_total),
                ),
                (
                    "connexa_lobby_requests_total",
                    "Joiners placed in a lobby",
                    get(&self.lobby_requests_total),
                ),
                (
                    "connexa_admitted_total",
                    "Lobby joiners admitted by the host",
                    get(&self.admitted_total),
                ),
                (
                    "connexa_denied_total",
                    "Lobby joiners denied by the host",
                    get(&self.denied_total),
                ),
                (
                    "connexa_pin_failures_total",
                    "Wrong session PINs entered",
                    get(&self.pin_failures_total),
                ),
                (
                    "connexa_rate_limited_total",
                    "Requests rejected by rate limiting",
                    get(&self.rate_limited_total),
                ),
                (
                    "connexa_relayed_total",
                    "SDP/ICE messages relayed between peers",
                    get(&self.relayed_total),
                ),
                (
                    "connexa_devices_verified_total",
                    "Device key proofs accepted",
                    get(&self.devices_verified_total),
                ),
            ],
            errors,
        }
    }
}
