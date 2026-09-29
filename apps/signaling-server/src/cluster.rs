//! Multi-node signaling.
//!
//! Every node has a slot 1–9, and the first digit of a room code is the slot of
//! the node that owns the room. A client may connect to any node (plain
//! round-robin load balancing, no sticky sessions): when it joins or resumes a
//! room owned elsewhere, its node forwards the client's messages to the owner
//! over a pub/sub bus and relays the owner's replies back.
//!
//! ```text
//! client ──ws──► node 2 (origin) ──bus: connexa:node:5──► node 5 (owner, runs the Hub room)
//! client ◄─ws─── node 2          ◄──bus: connexa:node:2── node 5
//! ```
//!
//! Nodes heartbeat on the bus. If an owner disappears its rooms are gone, so
//! the origin tells affected clients `room_ended { server_lost }`; if an origin
//! disappears, the owner treats its clients as disconnected.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use connexa_protocol::{ClientMessage, RoomEndReason, ServerMessage};
use connexa_security::normalize_room_code;
use connexa_signaling::{Connection, DeviceInfo, Hub};
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

const HEARTBEAT_EVERY: Duration = Duration::from_secs(3);
const HEARTBEAT_TTL: Duration = Duration::from_secs(10);
const LIVENESS_CHECK_EVERY: Duration = Duration::from_secs(5);

/// Publish/subscribe transport between nodes.
#[async_trait]
pub trait Bus: Send + Sync {
    async fn publish(&self, channel: &str, payload: String) -> Result<()>;
    async fn subscribe(&self, channel: &str) -> Result<mpsc::UnboundedReceiver<String>>;
    async fn heartbeat(&self, slot: u8, ttl: Duration) -> Result<()>;
    async fn alive(&self, slot: u8) -> Result<bool>;
}

pub fn node_channel(slot: u8) -> String {
    format!("connexa:node:{slot}")
}

#[derive(Serialize, Deserialize)]
struct WireDevice {
    id: String,
    name: String,
    platform: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "w", rename_all = "snake_case")]
enum Wire {
    /// Origin → owner: a client message (`None` = the client disconnected).
    Up {
        origin: u8,
        conn: u64,
        ip: Option<IpAddr>,
        device: Option<WireDevice>,
        msg: Option<ClientMessage>,
    },
    /// Owner → origin: a message for one of the origin's clients.
    Down { conn: u64, msg: ServerMessage },
}

pub struct Cluster {
    slot: u8,
    bus: Arc<dyn Bus>,
    hub: Arc<Hub>,
    /// Origin side: our WebSocket clients attached to rooms on other nodes.
    remote: DashMap<u64, RemoteClient>,
    /// Owner side: hub connections standing in for clients of other nodes.
    owned: DashMap<(u8, u64), Connection>,
}

struct RemoteClient {
    owner: u8,
    ws: mpsc::Sender<ServerMessage>,
}

impl Cluster {
    pub async fn start(slot: u8, bus: Arc<dyn Bus>, hub: Arc<Hub>) -> Result<Arc<Self>> {
        anyhow::ensure!((1..=9).contains(&slot), "cluster node slot must be 1-9");
        let cluster = Arc::new(Self {
            slot,
            bus: bus.clone(),
            hub,
            remote: DashMap::new(),
            owned: DashMap::new(),
        });
        let mut inbox = bus.subscribe(&node_channel(slot)).await?;
        bus.heartbeat(slot, HEARTBEAT_TTL).await?;

        let me = cluster.clone();
        tokio::spawn(async move {
            while let Some(payload) = inbox.recv().await {
                match serde_json::from_str::<Wire>(&payload) {
                    Ok(wire) => me.on_wire(wire).await,
                    Err(e) => warn!("dropping malformed cluster message: {e}"),
                }
            }
            warn!("cluster subscription ended");
        });

        let me = cluster.clone();
        tokio::spawn(async move {
            let mut beat = tokio::time::interval(HEARTBEAT_EVERY);
            let mut check = tokio::time::interval(LIVENESS_CHECK_EVERY);
            loop {
                tokio::select! {
                    _ = beat.tick() => {
                        if let Err(e) = me.bus.heartbeat(me.slot, HEARTBEAT_TTL).await {
                            warn!("cluster heartbeat failed: {e}");
                        }
                    }
                    _ = check.tick() => me.check_liveness().await,
                }
            }
        });
        info!(slot, "cluster node started");
        Ok(cluster)
    }

    pub fn slot(&self) -> u8 {
        self.slot
    }

    /// Which node owns the room this message refers to, if it is another node.
    pub fn remote_owner(&self, msg: &ClientMessage) -> Option<u8> {
        let code = match msg {
            ClientMessage::JoinRoom { room_id, .. } | ClientMessage::Resume { room_id, .. } => {
                normalize_room_code(room_id)?
            }
            _ => return None,
        };
        let owner = code.as_bytes()[0] - b'0';
        (owner != self.slot && !self.hub.has_room(&code)).then_some(owner)
    }

    /// Start proxying a local WebSocket client to `owner`.
    pub fn attach(&self, conn_id: u64, owner: u8, ws: mpsc::Sender<ServerMessage>) {
        self.remote.insert(conn_id, RemoteClient { owner, ws });
    }

    pub fn owner_of(&self, conn_id: u64) -> Option<u8> {
        self.remote.get(&conn_id).map(|r| r.owner)
    }

    pub async fn forward(
        &self,
        conn_id: u64,
        ip: Option<IpAddr>,
        device: Option<DeviceInfo>,
        msg: Option<ClientMessage>,
    ) {
        let Some(owner) = self.owner_of(conn_id) else {
            return;
        };
        if msg.is_none() {
            self.remote.remove(&conn_id);
        }
        let wire = Wire::Up {
            origin: self.slot,
            conn: conn_id,
            ip,
            device: device.map(|d| WireDevice {
                id: d.id,
                name: d.name,
                platform: d.platform,
            }),
            msg,
        };
        self.publish(owner, &wire).await;
    }

    async fn publish(&self, slot: u8, wire: &Wire) {
        let payload = serde_json::to_string(wire).expect("cluster messages serialize");
        if let Err(e) = self.bus.publish(&node_channel(slot), payload).await {
            warn!(slot, "cluster publish failed: {e}");
        }
    }

    async fn on_wire(self: &Arc<Self>, wire: Wire) {
        match wire {
            Wire::Down { conn, msg } => {
                if let Some(client) = self.remote.get(&conn)
                    && client.ws.try_send(msg).is_err()
                {
                    debug!(conn, "remote reply dropped: client gone or slow");
                }
            }
            Wire::Up {
                origin,
                conn,
                ip,
                device,
                msg,
            } => {
                let key = (origin, conn);
                let Some(msg) = msg else {
                    if let Some((_, c)) = self.owned.remove(&key) {
                        self.hub.disconnect(c);
                    }
                    return;
                };
                if !self.owned.contains_key(&key) {
                    let (tx, mut rx) = mpsc::channel::<ServerMessage>(256);
                    let device = device.map(|d| DeviceInfo {
                        id: d.id,
                        name: d.name,
                        platform: d.platform,
                    });
                    let c = self.hub.connect_with_device(tx, ip, device);
                    self.owned.insert(key, c);
                    let me = self.clone();
                    tokio::spawn(async move {
                        while let Some(msg) = rx.recv().await {
                            me.publish(origin, &Wire::Down { conn, msg }).await;
                        }
                    });
                }
                if let Some(mut c) = self.owned.get_mut(&key) {
                    self.hub.handle(&mut c, msg);
                }
            }
        }
    }

    async fn check_liveness(&self) {
        let mut slots: HashSet<u8> = self.remote.iter().map(|r| r.owner).collect();
        slots.extend(self.owned.iter().map(|e| e.key().0));
        let mut dead = HashMap::new();
        for slot in slots {
            let alive = self.bus.alive(slot).await.unwrap_or(true);
            dead.insert(slot, !alive);
        }
        let is_dead = |slot: &u8| dead.get(slot).copied().unwrap_or(false);

        let lost: Vec<u64> = self
            .remote
            .iter()
            .filter(|r| is_dead(&r.owner))
            .map(|r| *r.key())
            .collect();
        for conn in lost {
            if let Some((_, client)) = self.remote.remove(&conn) {
                warn!(owner = client.owner, "room owner node is gone");
                let _ = client.ws.try_send(ServerMessage::RoomEnded {
                    reason: RoomEndReason::ServerLost,
                });
            }
        }
        let orphaned: Vec<(u8, u64)> = self
            .owned
            .iter()
            .filter(|e| is_dead(&e.key().0))
            .map(|e| *e.key())
            .collect();
        for key in orphaned {
            if let Some((_, c)) = self.owned.remove(&key) {
                self.hub.disconnect(c);
            }
        }
    }
}

/// In-process bus: for tests and for running several nodes in one process.
#[derive(Default)]
pub struct MemoryBus {
    subscribers: DashMap<String, Vec<mpsc::UnboundedSender<String>>>,
    alive: DashMap<u8, Instant>,
    killed: DashMap<u8, ()>,
}

impl MemoryBus {
    /// Simulate a node crash (tests): it stops heartbeating and receiving.
    pub fn kill(&self, slot: u8) {
        self.killed.insert(slot, ());
        self.alive.remove(&slot);
        self.subscribers.remove(&node_channel(slot));
    }
}

#[async_trait]
impl Bus for MemoryBus {
    async fn publish(&self, channel: &str, payload: String) -> Result<()> {
        if let Some(mut subs) = self.subscribers.get_mut(channel) {
            subs.retain(|s| s.send(payload.clone()).is_ok());
        }
        Ok(())
    }

    async fn subscribe(&self, channel: &str) -> Result<mpsc::UnboundedReceiver<String>> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.subscribers
            .entry(channel.to_string())
            .or_default()
            .push(tx);
        Ok(rx)
    }

    async fn heartbeat(&self, slot: u8, ttl: Duration) -> Result<()> {
        if !self.killed.contains_key(&slot) {
            self.alive.insert(slot, Instant::now() + ttl);
        }
        Ok(())
    }

    async fn alive(&self, slot: u8) -> Result<bool> {
        Ok(self.alive.get(&slot).is_some_and(|t| *t > Instant::now()))
    }
}

/// Redis pub/sub bus (`CONNEXA_REDIS_URL`).
pub struct RedisBus {
    client: redis::Client,
    conn: redis::aio::MultiplexedConnection,
}

impl RedisBus {
    pub async fn connect(url: &str) -> Result<Self> {
        let client = redis::Client::open(url)?;
        let conn = client.get_multiplexed_async_connection().await?;
        Ok(Self { client, conn })
    }
}

#[async_trait]
impl Bus for RedisBus {
    async fn publish(&self, channel: &str, payload: String) -> Result<()> {
        use redis::AsyncCommands;
        let mut conn = self.conn.clone();
        let _: i64 = conn.publish(channel, payload).await?;
        Ok(())
    }

    async fn subscribe(&self, channel: &str) -> Result<mpsc::UnboundedReceiver<String>> {
        use futures_util::StreamExt;
        let mut pubsub = self.client.get_async_pubsub().await?;
        pubsub.subscribe(channel).await?;
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut stream = pubsub.into_on_message();
            while let Some(msg) = stream.next().await {
                if let Ok(payload) = msg.get_payload::<String>()
                    && tx.send(payload).is_err()
                {
                    break;
                }
            }
        });
        Ok(rx)
    }

    async fn heartbeat(&self, slot: u8, ttl: Duration) -> Result<()> {
        use redis::AsyncCommands;
        let mut conn = self.conn.clone();
        let _: () = conn
            .pset_ex(format!("connexa:alive:{slot}"), 1, ttl.as_millis() as u64)
            .await?;
        Ok(())
    }

    async fn alive(&self, slot: u8) -> Result<bool> {
        use redis::AsyncCommands;
        let mut conn = self.conn.clone();
        Ok(conn.exists(format!("connexa:alive:{slot}")).await?)
    }
}
