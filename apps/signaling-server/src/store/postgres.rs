//! Postgres store. Schema migrations are embedded and applied at startup.

use anyhow::{Context, Result};
use async_trait::async_trait;
use connexa_protocol::{ActivityEvent, DeviceSummary};
use connexa_signaling::{AuditRecord, DeviceInfo, unix_seconds};
use sqlx::Row;
use sqlx::postgres::{PgPool, PgPoolOptions};

use super::{Store, StoreError};

/// Applied in order; each statement is idempotent.
const MIGRATIONS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS devices (
        id          TEXT PRIMARY KEY,
        public_key  TEXT NOT NULL,
        name        TEXT NOT NULL,
        platform    TEXT NOT NULL,
        first_seen  TIMESTAMPTZ NOT NULL DEFAULT now(),
        last_seen   TIMESTAMPTZ NOT NULL DEFAULT now()
    )",
    "CREATE TABLE IF NOT EXISTS trusted_devices (
        owner       TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
        device      TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
        created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
        PRIMARY KEY (owner, device)
    )",
    "CREATE TABLE IF NOT EXISTS audit_events (
        id       BIGSERIAL PRIMARY KEY,
        at       TIMESTAMPTZ NOT NULL,
        kind     TEXT NOT NULL,
        room     TEXT NOT NULL,
        actor    TEXT,
        subject  TEXT
    )",
    "CREATE INDEX IF NOT EXISTS audit_events_actor ON audit_events (actor, at DESC)",
    "CREATE INDEX IF NOT EXISTS audit_events_subject ON audit_events (subject, at DESC)",
    "CREATE INDEX IF NOT EXISTS audit_events_at ON audit_events (at)",
];

pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    pub async fn connect(url: &str) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .acquire_timeout(std::time::Duration::from_secs(5))
            .connect(url)
            .await
            .context("connecting to Postgres")?;
        for sql in MIGRATIONS {
            sqlx::query(sql)
                .execute(&pool)
                .await
                .with_context(|| format!("migration failed: {sql}"))?;
        }
        Ok(Self { pool })
    }

    /// Empty all tables (tests only).
    pub async fn reset_for_tests(&self) -> Result<()> {
        sqlx::query("TRUNCATE audit_events, trusted_devices, devices")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl Store for PgStore {
    async fn upsert_device(&self, device: &DeviceInfo, public_key: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO devices (id, public_key, name, platform) VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO UPDATE SET name = EXCLUDED.name, platform = EXCLUDED.platform, last_seen = now()",
        )
        .bind(&device.id)
        .bind(public_key)
        .bind(&device.name)
        .bind(&device.platform)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn set_trust(&self, owner: &str, device: &str, trusted: bool) -> Result<()> {
        if trusted {
            let known: Option<(String,)> = sqlx::query_as("SELECT id FROM devices WHERE id = $1")
                .bind(device)
                .fetch_optional(&self.pool)
                .await?;
            if known.is_none() {
                return Err(StoreError::UnknownDevice.into());
            }
            sqlx::query(
                "INSERT INTO trusted_devices (owner, device) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            )
            .bind(owner)
            .bind(device)
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query("DELETE FROM trusted_devices WHERE owner = $1 AND device = $2")
                .bind(owner)
                .bind(device)
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }

    async fn trusted(&self, owner: &str) -> Result<Vec<DeviceSummary>> {
        let rows = sqlx::query(
            "SELECT d.id, d.name, d.platform, EXTRACT(EPOCH FROM d.last_seen)::BIGINT AS last_seen
             FROM trusted_devices t JOIN devices d ON d.id = t.device
             WHERE t.owner = $1 ORDER BY d.name",
        )
        .bind(owner)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| DeviceSummary {
                device_id: r.get("id"),
                name: r.get("name"),
                platform: r.get("platform"),
                last_seen: r.get::<i64, _>("last_seen").max(0) as u64,
            })
            .collect())
    }

    async fn record(&self, e: &AuditRecord) -> Result<()> {
        sqlx::query(
            "INSERT INTO audit_events (at, kind, room, actor, subject) VALUES (to_timestamp($1), $2, $3, $4, $5)",
        )
        .bind(unix_seconds(e.at) as f64)
        .bind(&e.kind)
        .bind(&e.room)
        .bind(&e.actor)
        .bind(&e.subject)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn activity(&self, device: &str, limit: usize) -> Result<Vec<ActivityEvent>> {
        let rows = sqlx::query(
            "SELECT EXTRACT(EPOCH FROM e.at)::BIGINT AS at, e.kind,
                    CASE WHEN a.id IS NULL THEN e.actor ELSE a.name || ' (' || a.id || ')' END AS actor,
                    CASE WHEN s.id IS NULL THEN e.subject ELSE s.name || ' (' || s.id || ')' END AS subject
             FROM audit_events e
             LEFT JOIN devices a ON a.id = e.actor
             LEFT JOIN devices s ON s.id = e.subject
             WHERE e.actor = $1 OR e.subject = $1
             ORDER BY e.at DESC, e.id DESC LIMIT $2",
        )
        .bind(device)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| ActivityEvent {
                at: r.get::<i64, _>("at").max(0) as u64,
                kind: r.get("kind"),
                actor: r.get("actor"),
                subject: r.get("subject"),
            })
            .collect())
    }

    async fn prune(&self, days: u32) -> Result<u64> {
        let done =
            sqlx::query("DELETE FROM audit_events WHERE at < now() - make_interval(days => $1)")
                .bind(days as i32)
                .execute(&self.pool)
                .await?;
        Ok(done.rows_affected())
    }

    fn kind(&self) -> &'static str {
        "postgres"
    }
}
