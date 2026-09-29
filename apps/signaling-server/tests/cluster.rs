//! Two server nodes sharing an in-memory bus: rooms are reachable from any node.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use signaling_server::cluster::{Bus, MemoryBus};
use signaling_server::{AppState, Config, Services, router};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn node(slot: u8, bus: Arc<MemoryBus>) -> SocketAddr {
    node_with(slot, bus).await
}

async fn node_with(slot: u8, bus: Arc<dyn Bus>) -> SocketAddr {
    let mut config = Config {
        node_slot: Some(slot),
        ..Default::default()
    };
    config.hub.code_prefix = Some(slot);
    let state = AppState::with_services(
        config,
        Services {
            bus: Some(bus),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = router(state).into_make_service_with_connect_info::<SocketAddr>();
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
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
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
async fn rooms_are_reachable_from_any_node() {
    let bus = Arc::new(MemoryBus::default());
    let node1 = node(1, bus.clone()).await;
    let node2 = node(2, bus.clone()).await;

    let mut host = connect(node1).await;
    send(
        &mut host,
        json!({"type": "create_room", "display_name": "Alice"}),
    )
    .await;
    let created = recv(&mut host).await;
    let code = created["room_id"].as_str().unwrap().to_string();
    let host_id = created["participant_id"].as_str().unwrap().to_string();
    assert!(
        code.starts_with('1'),
        "node 1 owns rooms starting with 1: {code}"
    );

    // Bob connects to node 2 and joins the room that lives on node 1.
    let mut guest = connect(node2).await;
    send(
        &mut guest,
        json!({"type": "join_room", "room_id": code, "display_name": "Bob"}),
    )
    .await;
    let joined = recv(&mut guest).await;
    assert_eq!(joined["type"], "room_joined", "{joined}");
    assert_eq!(joined["host_id"], host_id.as_str());
    let guest_id = joined["participant_id"].as_str().unwrap().to_string();
    assert_eq!(recv(&mut host).await["type"], "participant_joined");

    // Signaling flows both ways across nodes.
    send(
        &mut guest,
        json!({"type": "sdp_offer", "target": host_id, "sdp": "offer"}),
    )
    .await;
    let offer = recv(&mut host).await;
    assert_eq!(offer["from"], guest_id.as_str());
    send(
        &mut host,
        json!({"type": "sdp_answer", "target": guest_id, "sdp": "answer"}),
    )
    .await;
    assert_eq!(recv(&mut guest).await["sdp"], "answer");
    send(&mut guest, json!({"type": "ping"})).await;
    assert_eq!(recv(&mut guest).await["type"], "pong");

    // Metrics on the owner count the remote participant.
    let body = http_get(node1, "/metrics").await;
    assert!(body.contains("connexa_participants 2"), "{body}");
    assert!(body.contains("node_slot=\"1\""), "{body}");

    // Leaving through node 2 reaches the owner.
    send(&mut guest, json!({"type": "leave_room"})).await;
    assert_eq!(recv(&mut host).await["type"], "participant_left");
}

#[tokio::test]
async fn clients_learn_when_the_owner_node_dies() {
    let bus = Arc::new(MemoryBus::default());
    let node1 = node(1, bus.clone()).await;
    let node2 = node(2, bus.clone()).await;

    let mut host = connect(node1).await;
    send(&mut host, json!({"type": "create_room"})).await;
    let code = recv(&mut host).await["room_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut guest = connect(node2).await;
    send(&mut guest, json!({"type": "join_room", "room_id": code})).await;
    assert_eq!(recv(&mut guest).await["type"], "room_joined");

    bus.kill(1);
    let ended = recv(&mut guest).await;
    assert_eq!(ended["type"], "room_ended");
    assert_eq!(ended["reason"], "server_lost");
}

#[tokio::test]
async fn unknown_codes_on_other_nodes_report_not_found() {
    let bus = Arc::new(MemoryBus::default());
    let _node1 = node(1, bus.clone()).await;
    let node2 = node(2, bus.clone()).await;
    let mut c = connect(node2).await;
    send(&mut c, json!({"type": "join_room", "room_id": "123456789"})).await;
    assert_eq!(recv(&mut c).await["code"], "room_not_found");
}

async fn http_get(addr: SocketAddr, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut body = String::new();
    stream.read_to_string(&mut body).await.unwrap();
    body
}

/// Same flow over real Redis pub/sub; runs when `CONNEXA_TEST_REDIS_URL` is set (CI does).
#[tokio::test]
async fn redis_bus_connects_nodes() {
    let Ok(url) = std::env::var("CONNEXA_TEST_REDIS_URL") else {
        eprintln!("skipping: CONNEXA_TEST_REDIS_URL not set");
        return;
    };
    let bus1: Arc<dyn Bus> = Arc::new(
        signaling_server::cluster::RedisBus::connect(&url)
            .await
            .unwrap(),
    );
    let bus2: Arc<dyn Bus> = Arc::new(
        signaling_server::cluster::RedisBus::connect(&url)
            .await
            .unwrap(),
    );
    let node3 = node_with(3, bus1).await;
    let node4 = node_with(4, bus2).await;

    let mut host = connect(node3).await;
    send(&mut host, json!({"type": "create_room"})).await;
    let created = recv(&mut host).await;
    let code = created["room_id"].as_str().unwrap().to_string();
    assert!(code.starts_with('3'));
    let host_id = created["participant_id"].as_str().unwrap().to_string();

    let mut guest = connect(node4).await;
    send(&mut guest, json!({"type": "join_room", "room_id": code})).await;
    assert_eq!(recv(&mut guest).await["type"], "room_joined");
    assert_eq!(recv(&mut host).await["type"], "participant_joined");
    send(
        &mut guest,
        json!({"type": "sdp_offer", "target": host_id, "sdp": "via redis"}),
    )
    .await;
    assert_eq!(recv(&mut host).await["sdp"], "via redis");
}
