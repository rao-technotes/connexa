//! Connexa signaling server: WebSocket transport around [`connexa_signaling::Hub`],
//! plus static hosting of the web client, metrics, persistence (devices, trust,
//! audit), multi-node clustering and the SFU for large rooms.

pub mod cluster;
pub mod config;
pub mod sfu;
pub mod store;

use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use connexa_protocol::{
    ClientMessage, ErrorCode, PROTOCOL_VERSION, ServerMessage, decode_client, encode_server,
};
use connexa_security::constant_time_eq;
use connexa_signaling::{Connection, Hub, HubEvent, SfuSignal};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower_http::services::ServeDir;
use tracing::{debug, info, warn};

use cluster::{Bus, Cluster};
pub use config::Config;
use store::{MemoryStore, Store, StoreError};

/// Largest frame a client may send. SDP for a few tracks is well below this.
const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Outbound queue per connection; a client this far behind is dropped.
const OUTBOUND_QUEUE: usize = 256;
const ACTIVITY_LIMIT: usize = 50;

/// A media server for SFU rooms (implemented by `connexa-sfu`).
#[async_trait]
pub trait MediaServer: Send + Sync {
    async fn signal(&self, room: &str, participant: &str, signal: SfuSignal);
    async fn participant_left(&self, room: &str, participant: &str);
    async fn room_closed(&self, room: &str);
    /// (metric name, help, value) gauges for `/metrics`.
    fn gauges(&self) -> Vec<(&'static str, &'static str, u64)>;
}

/// Builds the media server once the hub exists (it sends through the hub).
pub type MediaFactory = Box<dyn FnOnce(Arc<Hub>) -> Arc<dyn MediaServer> + Send>;

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<Hub>,
    pub config: Arc<Config>,
    pub store: Arc<dyn Store>,
    pub cluster: Option<Arc<Cluster>>,
    pub media: Option<Arc<dyn MediaServer>>,
}

/// Everything optional an [`AppState`] can be assembled with.
#[derive(Default)]
pub struct Services {
    pub store: Option<Arc<dyn Store>>,
    pub bus: Option<Arc<dyn Bus>>,
    /// Builds the media server once the hub exists (it sends through the hub).
    pub media: Option<MediaFactory>,
}

impl AppState {
    /// Single node, in-memory store, no SFU. Must be called inside a Tokio runtime.
    pub fn new(config: Config) -> Self {
        Self::assemble_sync(config, Arc::new(MemoryStore::default()), None)
    }

    /// Full assembly: optional database, cluster bus and media server.
    pub async fn with_services(config: Config, services: Services) -> anyhow::Result<Self> {
        let store = services
            .store
            .unwrap_or_else(|| Arc::new(MemoryStore::default()));
        let mut state = Self::assemble_sync(config, store, services.media);
        if let Some(bus) = services.bus {
            let slot = state
                .config
                .node_slot
                .ok_or_else(|| anyhow::anyhow!("clustering needs a node slot"))?;
            state.cluster = Some(Cluster::start(slot, bus, state.hub.clone()).await?);
        }
        Ok(state)
    }

    fn assemble_sync(config: Config, store: Arc<dyn Store>, media: Option<MediaFactory>) -> Self {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let mut hub_config = config.hub.clone();
        hub_config.sfu_available = media.is_some();
        let hub = Arc::new(Hub::new(hub_config, config.ice.provider()).with_events(events_tx));
        let media = media.map(|build| build(hub.clone()));
        spawn_event_pump(events_rx, hub.clone(), store.clone(), media.clone());
        spawn_pruner(store.clone(), config.audit_retention_days);
        Self {
            hub,
            config: Arc::new(config),
            store,
            cluster: None,
            media,
        }
    }

    async fn dispatch(
        &self,
        conn: &mut Connection,
        ws: &mpsc::Sender<ServerMessage>,
        ip: IpAddr,
        remote: &mut Option<u8>,
        msg: ClientMessage,
    ) {
        match msg {
            ClientMessage::TrustDevice { .. }
            | ClientMessage::ListTrustedDevices
            | ClientMessage::GetActivity => return self.device_admin(conn, msg).await,
            // Device proofs are always checked by the node the client is connected to.
            ClientMessage::DeviceHello { .. } | ClientMessage::DeviceProof { .. } => {
                return self.hub.handle(conn, msg);
            }
            _ => {}
        }
        if let Some(cluster) = &self.cluster {
            if remote.is_none()
                && let Some(owner) = cluster.remote_owner(&msg)
            {
                cluster.attach(conn.id(), owner, ws.clone());
                *remote = Some(owner);
            }
            if remote.is_some() {
                let device = self.hub.device_of(conn);
                return cluster
                    .forward(conn.id(), Some(ip), device, Some(msg))
                    .await;
            }
        }
        self.hub.handle(conn, msg);
    }

    async fn device_admin(&self, conn: &Connection, msg: ClientMessage) {
        let Some(device) = self.hub.device_of(conn) else {
            return conn.send(ServerMessage::error(
                ErrorCode::NotVerified,
                "this device has not been verified",
            ));
        };
        let result = match msg {
            ClientMessage::TrustDevice { device_id, trusted } => {
                match self.store.set_trust(&device.id, &device_id, trusted).await {
                    Ok(()) => {
                        self.hub.set_trust(&device.id, &device_id, trusted);
                        self.trusted_list(&device.id).await
                    }
                    Err(e) if e.downcast_ref::<StoreError>().is_some() => {
                        return conn.send(ServerMessage::error(
                            ErrorCode::TargetNotFound,
                            e.to_string(),
                        ));
                    }
                    Err(e) => Err(e),
                }
            }
            ClientMessage::ListTrustedDevices => self.trusted_list(&device.id).await,
            ClientMessage::GetActivity => self
                .store
                .activity(&device.id, ACTIVITY_LIMIT)
                .await
                .map(|events| ServerMessage::Activity { events }),
            _ => return,
        };
        match result {
            Ok(reply) => conn.send(reply),
            Err(e) => {
                warn!("store error: {e:#}");
                conn.send(ServerMessage::error(
                    ErrorCode::Unavailable,
                    "device data is temporarily unavailable",
                ));
            }
        }
    }

    async fn trusted_list(&self, owner: &str) -> anyhow::Result<ServerMessage> {
        Ok(ServerMessage::TrustedDevices {
            devices: self.store.trusted(owner).await?,
        })
    }
}

/// Handles hub events: persistence and SFU signaling.
fn spawn_event_pump(
    mut events: mpsc::UnboundedReceiver<HubEvent>,
    hub: Arc<Hub>,
    store: Arc<dyn Store>,
    media: Option<Arc<dyn MediaServer>>,
) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                HubEvent::DeviceSeen { device, public_key } => {
                    if let Err(e) = store.upsert_device(&device, &public_key).await {
                        warn!("could not store device: {e:#}");
                    }
                    match store.trusted(&device.id).await {
                        Ok(list) => {
                            hub.load_trust(&device.id, list.into_iter().map(|d| d.device_id))
                        }
                        Err(e) => warn!("could not load trusted devices: {e:#}"),
                    }
                }
                HubEvent::Audit(record) => {
                    if let Err(e) = store.record(&record).await {
                        warn!("could not write audit event: {e:#}");
                    }
                }
                HubEvent::Sfu {
                    room,
                    participant,
                    signal,
                } => {
                    if let Some(m) = &media {
                        m.signal(&room, &participant, signal).await;
                    }
                }
                HubEvent::ParticipantLeft { room, participant } => {
                    if let Some(m) = &media {
                        m.participant_left(&room, &participant).await;
                    }
                }
                HubEvent::RoomClosed { room } => {
                    if let Some(m) = &media {
                        m.room_closed(&room).await;
                    }
                }
            }
        }
    });
}

fn spawn_pruner(store: Arc<dyn Store>, days: u32) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(6 * 60 * 60));
        loop {
            tick.tick().await;
            match store.prune(days).await {
                Ok(0) => {}
                Ok(n) => info!(deleted = n, "pruned old audit events"),
                Err(e) => warn!("audit pruning failed: {e:#}"),
            }
        }
    });
}

pub fn router(state: AppState) -> Router {
    let web = ServeDir::new(&state.config.web_root);
    Router::new()
        .route("/ws", get(ws_upgrade))
        .route("/healthz", get(health))
        .route("/metrics", get(metrics))
        .fallback_service(web)
        .with_state(state)
}

/// Periodically expire rooms and reap disconnected participants.
pub fn spawn_sweeper(hub: Arc<Hub>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            hub.sweep(Instant::now());
        }
    })
}

pub async fn serve(listener: TcpListener, state: AppState) -> std::io::Result<()> {
    serve_until(listener, state, async {
        let _ = tokio::signal::ctrl_c().await;
        info!("shutting down");
    })
    .await
}

/// Serve until `shutdown` resolves. Used by the desktop app's LAN mode.
pub async fn serve_until(
    listener: TcpListener,
    state: AppState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let sweeper = spawn_sweeper(state.hub.clone());
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await;
    sweeper.abort();
    result
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let stats = state.hub.stats();
    Json(serde_json::json!({
        "status": "ok",
        "protocol_version": PROTOCOL_VERSION,
        "rooms": stats.rooms,
        "participants": stats.participants,
        "node_slot": state.cluster.as_ref().map(|c| c.slot()),
        "store": state.store.kind(),
        "sfu": state.media.is_some(),
    }))
}

/// Prometheus text exposition.
async fn metrics(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = &state.config.metrics_token {
        let given = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        if !constant_time_eq(given, token) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    let stats = state.hub.stats();
    let snapshot = state.hub.metrics();
    let mut out = String::new();
    let mut metric = |name: &str, kind: &str, help: &str, value: u64| {
        let _ = writeln!(
            out,
            "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}"
        );
    };
    for (name, help, value) in snapshot.counters {
        metric(name, "counter", help, value);
    }
    metric("connexa_rooms", "gauge", "Active rooms", stats.rooms as u64);
    metric(
        "connexa_sfu_rooms",
        "gauge",
        "Active SFU rooms",
        stats.sfu_rooms as u64,
    );
    metric(
        "connexa_participants",
        "gauge",
        "Participants in rooms",
        stats.participants as u64,
    );
    metric(
        "connexa_lobby_waiting",
        "gauge",
        "Joiners waiting in lobbies",
        stats.waiting as u64,
    );
    metric(
        "connexa_connections",
        "gauge",
        "Open client connections",
        stats.connections as u64,
    );
    if let Some(media) = &state.media {
        for (name, help, value) in media.gauges() {
            metric(name, "gauge", help, value);
        }
    }
    let _ = writeln!(
        out,
        "# HELP connexa_errors_total Errors returned to clients\n# TYPE connexa_errors_total counter"
    );
    for (code, n) in snapshot.errors {
        let _ = writeln!(out, "connexa_errors_total{{code=\"{code}\"}} {n}");
    }
    let _ = writeln!(
        out,
        "# HELP connexa_info Build and deployment information\n# TYPE connexa_info gauge\nconnexa_info{{version=\"{}\",store=\"{}\",node_slot=\"{}\",sfu=\"{}\"}} 1",
        env!("CARGO_PKG_VERSION"),
        state.store.kind(),
        state
            .cluster
            .as_ref()
            .map(|c| c.slot().to_string())
            .unwrap_or_default(),
        state.media.is_some(),
    );
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out).into_response()
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    if !origin_allowed(&state.config.allowed_origins, &headers) {
        warn!("rejected WebSocket from disallowed origin");
        return StatusCode::FORBIDDEN.into_response();
    }
    let ip = client_ip(state.config.trust_proxy, &headers, addr);
    ws.max_message_size(MAX_FRAME_BYTES)
        .max_frame_size(MAX_FRAME_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state, ip))
}

fn origin_allowed(allowed: &[String], headers: &HeaderMap) -> bool {
    if allowed.is_empty() {
        return true;
    }
    headers
        .get(header::ORIGIN)
        .and_then(|o| o.to_str().ok())
        .is_some_and(|origin| allowed.iter().any(|a| a == origin))
}

fn client_ip(trust_proxy: bool, headers: &HeaderMap, addr: SocketAddr) -> IpAddr {
    if trust_proxy {
        let forwarded = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .and_then(|v| v.trim().parse().ok());
        if let Some(ip) = forwarded {
            return ip;
        }
    }
    addr.ip()
}

async fn handle_socket(socket: WebSocket, state: AppState, ip: IpAddr) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel(OUTBOUND_QUEUE);
    let mut conn = state.hub.connect(tx.clone(), Some(ip));
    // Set when this client's room lives on another cluster node.
    let mut remote: Option<u8> = None;
    debug!(conn = conn.id(), "socket opened");

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink
                .send(Message::Text(encode_server(&msg).into()))
                .await
                .is_err()
            {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let idle = state.config.socket_idle_timeout;
    loop {
        let frame = match tokio::time::timeout(idle, stream.next()).await {
            Ok(Some(Ok(frame))) => frame,
            Ok(Some(Err(e))) => {
                debug!(conn = conn.id(), "socket error: {e}");
                break;
            }
            Ok(None) => break,
            Err(_) => {
                debug!(conn = conn.id(), "socket idle, closing");
                break;
            }
        };
        match frame {
            Message::Text(text) => match decode_client(text.as_str()) {
                Ok(msg) => state.dispatch(&mut conn, &tx, ip, &mut remote, msg).await,
                Err(error) => conn.send(error),
            },
            Message::Binary(_) => conn.send(ServerMessage::error(
                ErrorCode::InvalidMessage,
                "binary frames are not supported",
            )),
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    debug!(conn = conn.id(), "socket closed");
    if remote.is_some()
        && let Some(cluster) = &state.cluster
    {
        cluster.forward(conn.id(), None, None, None).await;
    }
    state.hub.disconnect(conn);
    drop(tx);
    // Give the writer a moment to flush final messages, then stop it.
    let _ = tokio::time::timeout(Duration::from_secs(1), writer).await;
}
