use super::*;
use tokio::sync::mpsc::Receiver;

struct Client {
    conn: Connection,
    rx: Receiver<ServerMessage>,
}

impl Client {
    fn send(&mut self, hub: &Hub, msg: ClientMessage) {
        hub.handle(&mut self.conn, msg);
    }

    fn recv(&mut self) -> ServerMessage {
        self.rx.try_recv().expect("expected a message")
    }

    fn assert_empty(&mut self) {
        if let Ok(msg) = self.rx.try_recv() {
            panic!("unexpected message {msg:?}");
        }
    }
}

fn hub_with(config: HubConfig) -> Hub {
    Hub::new(
        config,
        Arc::new(|_| {
            vec![IceServer {
                urls: vec!["stun:stun.example.org:3478".into()],
                username: None,
                credential: None,
            }]
        }),
    )
}

fn hub() -> Hub {
    hub_with(HubConfig::default())
}

fn client(hub: &Hub) -> Client {
    client_from(hub, "10.0.0.1")
}

fn client_from(hub: &Hub, ip: &str) -> Client {
    let (tx, rx) = mpsc::channel(64);
    Client {
        conn: hub.connect(tx, Some(ip.parse().unwrap())),
        rx,
    }
}

/// Host creates a room; returns (host, room code, host id, resume token).
fn create(hub: &Hub) -> (Client, String, String, String) {
    let mut host = client(hub);
    host.send(
        hub,
        ClientMessage::CreateRoom {
            display_name: Some("Alice".into()),
        },
    );
    match host.recv() {
        ServerMessage::RoomCreated {
            room_id,
            participant_id,
            resume_token,
            ice_servers,
            ..
        } => {
            assert_eq!(ice_servers.len(), 1);
            (host, room_id, participant_id, resume_token)
        }
        other => panic!("unexpected {other:?}"),
    }
}

fn join(hub: &Hub, code: &str, name: &str) -> (Client, String) {
    let mut c = client(hub);
    c.send(
        hub,
        ClientMessage::JoinRoom {
            room_id: code.into(),
            display_name: Some(name.into()),
        },
    );
    match c.recv() {
        ServerMessage::RoomJoined { participant_id, .. } => (c, participant_id),
        other => panic!("unexpected {other:?}"),
    }
}

fn expect_error(c: &mut Client, code: ErrorCode) {
    match c.recv() {
        ServerMessage::Error { code: got, .. } => assert_eq!(got, code),
        other => panic!("expected {code:?}, got {other:?}"),
    }
}

#[test]
fn create_and_join_announces_presence() {
    let hub = hub();
    let (mut host, code, host_id, _) = create(&hub);
    assert!(connexa_security::validate_room_id(&code));

    let mut guest = client(&hub);
    // Formatted input is accepted.
    let spaced = connexa_security::format_room_code(&code);
    guest.send(
        &hub,
        ClientMessage::JoinRoom {
            room_id: spaced,
            display_name: Some("Bob".into()),
        },
    );
    let guest_id = match guest.recv() {
        ServerMessage::RoomJoined {
            participant_id,
            host_id: h,
            participants,
            ..
        } => {
            assert_eq!(h, host_id);
            assert_eq!(participants.len(), 1);
            assert_eq!(participants[0].display_name, "Alice");
            assert!(participants[0].is_host);
            participant_id
        }
        other => panic!("unexpected {other:?}"),
    };

    match host.recv() {
        ServerMessage::ParticipantJoined { participant } => {
            assert_eq!(participant.participant_id, guest_id);
            assert_eq!(participant.display_name, "Bob");
            assert!(!participant.is_host);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        hub.stats(),
        HubStats {
            rooms: 1,
            participants: 2
        }
    );
}

#[test]
fn join_errors() {
    let hub = hub();
    let mut c = client(&hub);
    c.send(
        &hub,
        ClientMessage::JoinRoom {
            room_id: "12345".into(),
            display_name: None,
        },
    );
    expect_error(&mut c, ErrorCode::InvalidRoomCode);
    c.send(
        &hub,
        ClientMessage::JoinRoom {
            room_id: "123456789".into(),
            display_name: None,
        },
    );
    expect_error(&mut c, ErrorCode::RoomNotFound);
}

#[test]
fn room_capacity_is_enforced() {
    let hub = hub_with(HubConfig {
        max_participants: 2,
        ..Default::default()
    });
    let (_host, code, ..) = create(&hub);
    let _b = join(&hub, &code, "B");
    let mut c = client(&hub);
    c.send(
        &hub,
        ClientMessage::JoinRoom {
            room_id: code,
            display_name: None,
        },
    );
    expect_error(&mut c, ErrorCode::RoomFull);
}

#[test]
fn sdp_and_ice_are_relayed_only_to_target() {
    let hub = hub();
    let (mut a, code, a_id, _) = create(&hub);
    let (mut b, b_id) = join(&hub, &code, "B");
    let (mut c, _c_id) = join(&hub, &code, "C");
    a.recv(); // B joined
    a.recv(); // C joined
    b.recv(); // C joined

    c.send(
        &hub,
        ClientMessage::SdpOffer {
            target: a_id.clone(),
            sdp: "offer".into(),
        },
    );
    c.send(
        &hub,
        ClientMessage::IceCandidate {
            target: b_id.clone(),
            candidate: serde_json_candidate(),
        },
    );

    match a.recv() {
        ServerMessage::SdpOffer { from, sdp } => {
            assert_eq!(sdp, "offer");
            assert_ne!(from, a_id);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(matches!(b.recv(), ServerMessage::IceCandidate { .. }));
    a.assert_empty();
    b.assert_empty();
    c.assert_empty();
}

fn serde_json_candidate() -> serde_json::Value {
    serde_json::json!({ "candidate": "candidate:1 1 udp 1 1.2.3.4 5 typ host", "sdpMid": "0" })
}

#[test]
fn relay_to_unknown_or_self_fails() {
    let hub = hub();
    let (mut a, _code, a_id, _) = create(&hub);
    a.send(
        &hub,
        ClientMessage::SdpAnswer {
            target: "nobody".into(),
            sdp: String::new(),
        },
    );
    expect_error(&mut a, ErrorCode::TargetNotFound);
    a.send(
        &hub,
        ClientMessage::SdpAnswer {
            target: a_id,
            sdp: String::new(),
        },
    );
    expect_error(&mut a, ErrorCode::TargetNotFound);

    let mut outsider = client(&hub);
    outsider.send(
        &hub,
        ClientMessage::SdpOffer {
            target: "x".into(),
            sdp: String::new(),
        },
    );
    expect_error(&mut outsider, ErrorCode::NotInRoom);
}

#[test]
fn guest_leaving_notifies_others() {
    let hub = hub();
    let (mut a, code, ..) = create(&hub);
    let (mut b, b_id) = join(&hub, &code, "B");
    a.recv();
    b.send(&hub, ClientMessage::LeaveRoom);
    match a.recv() {
        ServerMessage::ParticipantLeft { participant_id } => assert_eq!(participant_id, b_id),
        other => panic!("unexpected {other:?}"),
    }
    b.assert_empty();
    assert_eq!(hub.stats().participants, 1);
}

#[test]
fn host_leaving_ends_room() {
    let hub = hub();
    let (mut a, code, ..) = create(&hub);
    let (mut b, _) = join(&hub, &code, "B");
    a.recv();
    a.send(&hub, ClientMessage::LeaveRoom);
    assert_eq!(
        b.recv(),
        ServerMessage::RoomEnded {
            reason: RoomEndReason::HostLeft
        }
    );
    assert_eq!(hub.stats().rooms, 0);
}

#[test]
fn only_host_can_end_room() {
    let hub = hub();
    let (mut a, code, ..) = create(&hub);
    let (mut b, _) = join(&hub, &code, "B");
    a.recv();
    b.send(&hub, ClientMessage::EndRoom);
    expect_error(&mut b, ErrorCode::NotHost);
    a.send(&hub, ClientMessage::EndRoom);
    assert_eq!(
        a.recv(),
        ServerMessage::RoomEnded {
            reason: RoomEndReason::HostEnded
        }
    );
    assert_eq!(
        b.recv(),
        ServerMessage::RoomEnded {
            reason: RoomEndReason::HostEnded
        }
    );
    // The ended room's code is no longer joinable.
    let mut c = client(&hub);
    c.send(
        &hub,
        ClientMessage::JoinRoom {
            room_id: code,
            display_name: None,
        },
    );
    expect_error(&mut c, ErrorCode::RoomNotFound);
}

#[test]
fn cannot_create_twice() {
    let hub = hub();
    let (mut a, ..) = create(&hub);
    a.send(&hub, ClientMessage::CreateRoom { display_name: None });
    expect_error(&mut a, ErrorCode::AlreadyInRoom);
}

#[test]
fn resume_replays_messages_missed_while_disconnected() {
    let hub = hub();
    let (mut a, code, a_id, token) = create(&hub);
    let (mut b, b_id) = join(&hub, &code, "B");
    a.recv();

    hub.disconnect(a.conn);
    b.send(
        &hub,
        ClientMessage::SdpOffer {
            target: a_id.clone(),
            sdp: "late offer".into(),
        },
    );
    b.assert_empty(); // a is still considered present

    let mut a2 = client(&hub);
    a2.send(
        &hub,
        ClientMessage::Resume {
            room_id: code.clone(),
            participant_id: a_id.clone(),
            resume_token: "wrong".into(),
        },
    );
    expect_error(&mut a2, ErrorCode::ResumeFailed);

    a2.send(
        &hub,
        ClientMessage::Resume {
            room_id: code,
            participant_id: a_id,
            resume_token: token,
        },
    );
    match a2.recv() {
        ServerMessage::SessionResumed { participants, .. } => {
            assert_eq!(participants[0].participant_id, b_id)
        }
        other => panic!("unexpected {other:?}"),
    }
    match a2.recv() {
        ServerMessage::SdpOffer { sdp, from } => {
            assert_eq!(sdp, "late offer");
            assert_eq!(from, b_id);
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn sweep_removes_participants_after_grace() {
    let hub = hub_with(HubConfig {
        reconnect_grace: Duration::from_secs(5),
        ..Default::default()
    });
    let (mut a, code, ..) = create(&hub);
    let (b, b_id) = join(&hub, &code, "B");
    a.recv();
    hub.disconnect(b.conn);

    hub.sweep(Instant::now());
    a.assert_empty();
    hub.sweep(Instant::now() + Duration::from_secs(6));
    assert_eq!(
        a.recv(),
        ServerMessage::ParticipantLeft {
            participant_id: b_id
        }
    );
}

#[test]
fn sweep_ends_room_when_host_does_not_return() {
    let hub = hub_with(HubConfig {
        reconnect_grace: Duration::from_secs(5),
        ..Default::default()
    });
    let (a, code, ..) = create(&hub);
    let (mut b, _) = join(&hub, &code, "B");
    hub.disconnect(a.conn);
    hub.sweep(Instant::now() + Duration::from_secs(6));
    assert_eq!(
        b.recv(),
        ServerMessage::RoomEnded {
            reason: RoomEndReason::HostLeft
        }
    );
    assert_eq!(hub.stats().rooms, 0);
}

#[test]
fn idle_rooms_expire() {
    let hub = hub_with(HubConfig {
        room_idle_timeout: Duration::from_secs(60),
        ..Default::default()
    });
    let (mut a, ..) = create(&hub);
    hub.sweep(Instant::now() + Duration::from_secs(61));
    assert_eq!(
        a.recv(),
        ServerMessage::RoomEnded {
            reason: RoomEndReason::Expired
        }
    );
    assert_eq!(hub.stats().rooms, 0);
}

#[test]
fn join_attempts_are_rate_limited_per_ip() {
    let hub = hub_with(HubConfig {
        join_attempts_per_window: 3,
        ..Default::default()
    });
    let mut guesser = client_from(&hub, "203.0.113.9");
    for _ in 0..3 {
        guesser.send(
            &hub,
            ClientMessage::JoinRoom {
                room_id: "123456789".into(),
                display_name: None,
            },
        );
        expect_error(&mut guesser, ErrorCode::RoomNotFound);
    }
    guesser.send(
        &hub,
        ClientMessage::JoinRoom {
            room_id: "123456789".into(),
            display_name: None,
        },
    );
    expect_error(&mut guesser, ErrorCode::RateLimited);

    let mut other = client_from(&hub, "203.0.113.10");
    other.send(
        &hub,
        ClientMessage::JoinRoom {
            room_id: "123456789".into(),
            display_name: None,
        },
    );
    expect_error(&mut other, ErrorCode::RoomNotFound);
}
