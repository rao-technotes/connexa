//! End-to-end test over real WebSockets.

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use signaling_server::{AppState, Config, router};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn start() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(AppState::new(Config::default()))
        .into_make_service_with_connect_info::<SocketAddr>();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

async fn connect(addr: SocketAddr) -> Ws {
    connect_async(format!("ws://{addr}/ws")).await.unwrap().0
}

async fn send(ws: &mut Ws, value: Value) {
    ws.send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}

async fn recv(ws: &mut Ws) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out")
            .unwrap()
            .unwrap();
        if let Message::Text(t) = msg {
            return serde_json::from_str(t.as_str()).unwrap();
        }
    }
}

#[tokio::test]
async fn two_clients_negotiate_through_the_server() {
    let addr = start().await;
    let mut host = connect(addr).await;
    let mut guest = connect(addr).await;

    send(
        &mut host,
        json!({"version": 1, "type": "create_room", "display_name": "Alice"}),
    )
    .await;
    let created = recv(&mut host).await;
    assert_eq!(created["type"], "room_created");
    assert_eq!(created["version"], 1);
    let code = created["room_id"].as_str().unwrap().to_string();
    let host_id = created["participant_id"].as_str().unwrap().to_string();
    assert_eq!(code.len(), 9);
    assert!(!created["ice_servers"].as_array().unwrap().is_empty());

    send(
        &mut guest,
        json!({"version": 1, "type": "join_room", "room_id": code}),
    )
    .await;
    let joined = recv(&mut guest).await;
    assert_eq!(joined["type"], "room_joined");
    assert_eq!(joined["host_id"], host_id.as_str());
    let guest_id = joined["participant_id"].as_str().unwrap().to_string();

    let presence = recv(&mut host).await;
    assert_eq!(presence["type"], "participant_joined");
    assert_eq!(presence["participant"]["participant_id"], guest_id.as_str());

    send(
        &mut guest,
        json!({"version": 1, "type": "sdp_offer", "target": host_id, "sdp": "v=0"}),
    )
    .await;
    let offer = recv(&mut host).await;
    assert_eq!(
        offer,
        json!({"version": 1, "type": "sdp_offer", "from": guest_id, "sdp": "v=0"})
    );

    send(
        &mut host,
        json!({"version": 1, "type": "sdp_answer", "target": guest_id, "sdp": "v=0 answer"}),
    )
    .await;
    assert_eq!(recv(&mut guest).await["type"], "sdp_answer");

    send(&mut guest, json!({"version": 1, "type": "leave_room"})).await;
    let left = recv(&mut host).await;
    assert_eq!(left["type"], "participant_left");
    assert_eq!(left["participant_id"], guest_id.as_str());
}

#[tokio::test]
async fn malformed_and_future_frames_get_errors() {
    let addr = start().await;
    let mut ws = connect(addr).await;

    ws.send(Message::Text("not json".into())).await.unwrap();
    assert_eq!(recv(&mut ws).await["code"], "invalid_message");

    send(&mut ws, json!({"version": 2, "type": "ping"})).await;
    assert_eq!(recv(&mut ws).await["code"], "unsupported_version");

    send(&mut ws, json!({"version": 1, "type": "ping"})).await;
    assert_eq!(recv(&mut ws).await["type"], "pong");
}

#[tokio::test]
async fn health_endpoint_reports_counts() {
    let addr = start().await;
    let mut ws = connect(addr).await;
    send(&mut ws, json!({"type": "create_room"})).await;
    recv(&mut ws).await;

    let mut stream = TcpStream::connect(addr).await.unwrap();
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut body = String::new();
    stream.read_to_string(&mut body).await.unwrap();
    assert!(body.contains("\"rooms\":1"), "{body}");
}
