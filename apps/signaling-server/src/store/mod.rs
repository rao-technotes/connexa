//! Persistence for device identities, trust relationships and the audit log.
//!
//! [`MemoryStore`] is used when no database is configured (development, LAN
//! mode); [`postgres::PgStore`] is used with `CONNEXA_DATABASE_URL`.

pub mod postgres;

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;
use connexa_protocol::{ActivityEvent, DeviceSummary};
use connexa_signaling::{AuditRecord, DeviceInfo, unix_seconds};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("that device has never connected to this server")]
    UnknownDevice,
}

#[async_trait]
pub trait Store: Send + Sync {
    async fn upsert_device(&self, device: &DeviceInfo, public_key: &str) -> Result<()>;
    /// Fails with [`StoreError::UnknownDevice`] when trusting a device never seen.
    async fn set_trust(&self, owner: &str, device: &str, trusted: bool) -> Result<()>;
    async fn trusted(&self, owner: &str) -> Result<Vec<DeviceSummary>>;
    async fn record(&self, event: &AuditRecord) -> Result<()>;
    /// Most recent events where `device` is the actor or the subject.
    async fn activity(&self, device: &str, limit: usize) -> Result<Vec<ActivityEvent>>;
    /// Delete audit events older than `days`.
    async fn prune(&self, days: u32) -> Result<u64>;
    fn kind(&self) -> &'static str;
}

#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<Memory>,
}

#[derive(Default)]
struct Memory {
    devices: HashMap<String, (DeviceInfo, u64)>,
    trust: HashMap<String, HashSet<String>>,
    audit: Vec<AuditRecord>,
}

const MAX_MEMORY_AUDIT: usize = 10_000;

impl Memory {
    fn label(&self, id: &Option<String>) -> Option<String> {
        id.as_ref().map(|id| match self.devices.get(id) {
            Some((d, _)) => format!("{} ({id})", d.name),
            None => id.clone(),
        })
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn upsert_device(&self, device: &DeviceInfo, _public_key: &str) -> Result<()> {
        let now = unix_seconds(std::time::SystemTime::now());
        self.inner
            .lock()
            .unwrap()
            .devices
            .insert(device.id.clone(), (device.clone(), now));
        Ok(())
    }

    async fn set_trust(&self, owner: &str, device: &str, trusted: bool) -> Result<()> {
        let mut m = self.inner.lock().unwrap();
        if trusted && !m.devices.contains_key(device) {
            return Err(StoreError::UnknownDevice.into());
        }
        let set = m.trust.entry(owner.to_string()).or_default();
        if trusted {
            set.insert(device.to_string());
        } else {
            set.remove(device);
        }
        Ok(())
    }

    async fn trusted(&self, owner: &str) -> Result<Vec<DeviceSummary>> {
        let m = self.inner.lock().unwrap();
        let mut out: Vec<DeviceSummary> = m
            .trust
            .get(owner)
            .into_iter()
            .flatten()
            .filter_map(|id| m.devices.get(id))
            .map(|(d, seen)| DeviceSummary {
                device_id: d.id.clone(),
                name: d.name.clone(),
                platform: d.platform.clone(),
                last_seen: *seen,
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn record(&self, event: &AuditRecord) -> Result<()> {
        let mut m = self.inner.lock().unwrap();
        if m.audit.len() >= MAX_MEMORY_AUDIT {
            m.audit.remove(0);
        }
        m.audit.push(event.clone());
        Ok(())
    }

    async fn activity(&self, device: &str, limit: usize) -> Result<Vec<ActivityEvent>> {
        let m = self.inner.lock().unwrap();
        Ok(m.audit
            .iter()
            .rev()
            .filter(|e| e.actor.as_deref() == Some(device) || e.subject.as_deref() == Some(device))
            .take(limit)
            .map(|e| ActivityEvent {
                at: unix_seconds(e.at),
                kind: e.kind.clone(),
                actor: m.label(&e.actor),
                subject: m.label(&e.subject),
            })
            .collect())
    }

    async fn prune(&self, days: u32) -> Result<u64> {
        let cutoff =
            std::time::SystemTime::now() - std::time::Duration::from_secs(u64::from(days) * 86_400);
        let mut m = self.inner.lock().unwrap();
        let before = m.audit.len();
        m.audit.retain(|e| e.at >= cutoff);
        Ok((before - m.audit.len()) as u64)
    }

    fn kind(&self) -> &'static str {
        "memory"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn dev(id: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            id: id.into(),
            name: name.into(),
            platform: "test".into(),
        }
    }

    pub async fn exercise(store: &dyn Store) {
        store
            .upsert_device(&dev("aaaa-1", "Laptop"), "pk1")
            .await
            .unwrap();
        store
            .upsert_device(&dev("bbbb-2", "Phone"), "pk2")
            .await
            .unwrap();

        assert!(store.set_trust("aaaa-1", "zzzz-9", true).await.is_err());
        store.set_trust("aaaa-1", "bbbb-2", true).await.unwrap();
        store.set_trust("aaaa-1", "bbbb-2", true).await.unwrap(); // idempotent
        let trusted = store.trusted("aaaa-1").await.unwrap();
        assert_eq!(trusted.len(), 1);
        assert_eq!(trusted[0].name, "Phone");
        store.set_trust("aaaa-1", "bbbb-2", false).await.unwrap();
        assert!(store.trusted("aaaa-1").await.unwrap().is_empty());

        store
            .record(&AuditRecord {
                at: SystemTime::now(),
                kind: "control_granted".into(),
                room: "r".into(),
                actor: Some("aaaa-1".into()),
                subject: Some("bbbb-2".into()),
            })
            .await
            .unwrap();
        let a = store.activity("bbbb-2", 10).await.unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, "control_granted");
        assert!(a[0].actor.as_deref().unwrap().contains("Laptop"));
        assert!(store.activity("cccc-3", 10).await.unwrap().is_empty());
        assert_eq!(store.prune(30).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn memory_store_behaves() {
        exercise(&MemoryStore::default()).await;
    }

    /// Runs against a real Postgres when `CONNEXA_TEST_DATABASE_URL` is set (CI does this).
    #[tokio::test]
    async fn postgres_store_behaves() {
        let Ok(url) = std::env::var("CONNEXA_TEST_DATABASE_URL") else {
            eprintln!("skipping: CONNEXA_TEST_DATABASE_URL not set");
            return;
        };
        let store = postgres::PgStore::connect(&url).await.unwrap();
        store.reset_for_tests().await.unwrap();
        exercise(&store).await;
    }
}
