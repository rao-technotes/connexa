//! Connexa signaling server: WebSocket transport around [`connexa_signaling::Hub`],
//! plus static hosting of the web client.

pub mod config;

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use connexa_protocol::{PROTOCOL_VERSION, decode_client, encode_server};
use connexa_signaling::Hub;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tower_http::services::ServeDir;
use tracing::{debug, info, warn};

pub use config::Config;

/// Largest frame a client may send. SDP for a few tracks is well below this.
const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Outbound queue per connection; a client this far behind is dropped.
const OUTBOUND_QUEUE: usize = 256;

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<Hub>,
    pub config: Arc<Config>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let hub = Hub::new(config.hub.clone(), config.ice.provider());
        Self {
            hub: Arc::new(hub),
            config: Arc::new(config),
        }
    }
}

pub fn router(state: AppState) -> Router {
    let web = ServeDir::new(&state.config.web_root);
    Router::new()
        .route("/ws", get(ws_upgrade))
        .route("/healthz", get(health))
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
    }))
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
    let mut conn = state.hub.connect(tx, Some(ip));
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
                Ok(msg) => state.hub.handle(&mut conn, msg),
                Err(error) => conn.send(error),
            },
            Message::Binary(_) => conn.send(connexa_protocol::ServerMessage::error(
                connexa_protocol::ErrorCode::InvalidMessage,
                "binary frames are not supported",
            )),
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    debug!(conn = conn.id(), "socket closed");
    state.hub.disconnect(conn);
    // Give the writer a moment to flush final messages, then stop it.
    let _ = tokio::time::timeout(Duration::from_secs(1), writer).await;
}
