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
            pin: None,
            lobby: false,
            large: false,
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
            pin: None,
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
            pin: None,
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
    let stats = hub.stats();
    assert_eq!(
        (stats.rooms, stats.participants, stats.connections),
        (1, 2, 2)
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
            pin: None,
        },
    );
    expect_error(&mut c, ErrorCode::InvalidRoomCode);
    c.send(
        &hub,
        ClientMessage::JoinRoom {
            room_id: "123456789".into(),
            display_name: None,
            pin: None,
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
            pin: None,
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
            pin: None,
        },
    );
    expect_error(&mut c, ErrorCode::RoomNotFound);
}

#[test]
fn cannot_create_twice() {
    let hub = hub();
    let (mut a, ..) = create(&hub);
    a.send(
        &hub,
        ClientMessage::CreateRoom {
            display_name: None,
            pin: None,
            lobby: false,
            large: false,
        },
    );
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
                pin: None,
            },
        );
        expect_error(&mut guesser, ErrorCode::RoomNotFound);
    }
    guesser.send(
        &hub,
        ClientMessage::JoinRoom {
            room_id: "123456789".into(),
            display_name: None,
            pin: None,
        },
    );
    expect_error(&mut guesser, ErrorCode::RateLimited);

    let mut other = client_from(&hub, "203.0.113.10");
    other.send(
        &hub,
        ClientMessage::JoinRoom {
            room_id: "123456789".into(),
            display_name: None,
            pin: None,
        },
    );
    expect_error(&mut other, ErrorCode::RoomNotFound);
}

// ----- lobby, PIN, devices, audit, SFU ---------------------------------------------

fn create_msg(pin: Option<&str>, lobby: bool, large: bool) -> ClientMessage {
    ClientMessage::CreateRoom {
        display_name: Some("Alice".into()),
        pin: pin.map(String::from),
        lobby,
        large,
    }
}

fn join_msg(code: &str, name: &str, pin: Option<&str>) -> ClientMessage {
    ClientMessage::JoinRoom {
        room_id: code.into(),
        display_name: Some(name.into()),
        pin: pin.map(String::from),
    }
}

fn created(c: &mut Client) -> (String, String) {
    match c.recv() {
        ServerMessage::RoomCreated {
            room_id,
            participant_id,
            ..
        } => (room_id, participant_id),
        other => panic!("unexpected {other:?}"),
    }
}

fn events_hub(config: HubConfig) -> (Hub, mpsc::UnboundedReceiver<HubEvent>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (hub_with(config).with_events(tx), rx)
}

fn drain(rx: &mut mpsc::UnboundedReceiver<HubEvent>) -> Vec<HubEvent> {
    let mut v = Vec::new();
    while let Ok(e) = rx.try_recv() {
        v.push(e);
    }
    v
}

/// Run the device key proof for a client; returns its device id.
fn verify_device(hub: &Hub, c: &mut Client, seed: u8) -> String {
    use base64::Engine;
    use p256::ecdsa::signature::Signer;
    use p256::ecdsa::{Signature, SigningKey};
    use p256::pkcs8::EncodePublicKey;
    let key = SigningKey::from_slice(&[seed; 32]).unwrap();
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let public = b64(key.verifying_key().to_public_key_der().unwrap().as_bytes());
    c.send(
        hub,
        ClientMessage::DeviceHello {
            public_key: public,
            name: Some(format!("device {seed}")),
            platform: Some("test".into()),
        },
    );
    let nonce = match c.recv() {
        ServerMessage::DeviceChallenge { nonce } => nonce,
        other => panic!("unexpected {other:?}"),
    };
    let sig: Signature = key.sign(format!("connexa-device-auth:{nonce}").as_bytes());
    c.send(
        hub,
        ClientMessage::DeviceProof {
            signature: b64(&sig.to_bytes()),
        },
    );
    match c.recv() {
        ServerMessage::DeviceVerified { device_id } => device_id,
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn lobby_joiners_wait_until_admitted() {
    let hub = hub();
    let mut host = client(&hub);
    host.send(&hub, create_msg(None, true, false));
    let (code, _) = created(&mut host);

    let mut bob = client(&hub);
    bob.send(&hub, join_msg(&code, "Bob", None));
    assert_eq!(
        bob.recv(),
        ServerMessage::LobbyWaiting {
            room_id: code.clone()
        }
    );
    let request_id = match host.recv() {
        ServerMessage::JoinRequest {
            request_id,
            display_name,
            ..
        } => {
            assert_eq!(display_name, "Bob");
            request_id
        }
        other => panic!("unexpected {other:?}"),
    };
    // Waiting does not count as being in the room.
    assert_eq!(hub.stats().participants, 1);
    bob.send(
        &hub,
        ClientMessage::SdpOffer {
            target: "x".into(),
            sdp: String::new(),
        },
    );
    expect_error(&mut bob, ErrorCode::NotInRoom);

    host.send(
        &hub,
        ClientMessage::Admit {
            request_id: request_id.clone(),
        },
    );
    assert!(matches!(
        host.recv(),
        ServerMessage::ParticipantJoined { .. }
    ));
    assert!(matches!(bob.recv(), ServerMessage::RoomJoined { .. }));
    assert_eq!(hub.stats().participants, 2);

    // A stale request cannot be answered twice.
    host.send(&hub, ClientMessage::Admit { request_id });
    expect_error(&mut host, ErrorCode::TargetNotFound);
}

#[test]
fn lobby_deny_and_cancel() {
    let hub = hub();
    let mut host = client(&hub);
    host.send(&hub, create_msg(None, true, false));
    let (code, _) = created(&mut host);

    let mut bob = client(&hub);
    bob.send(&hub, join_msg(&code, "Bob", None));
    bob.recv();
    let ServerMessage::JoinRequest { request_id, .. } = host.recv() else {
        panic!()
    };
    host.send(&hub, ClientMessage::Deny { request_id });
    expect_error(&mut bob, ErrorCode::JoinDenied);
    // Bob is free to try again (and wait again).
    bob.send(&hub, join_msg(&code, "Bob", None));
    assert!(matches!(bob.recv(), ServerMessage::LobbyWaiting { .. }));
    let ServerMessage::JoinRequest { request_id, .. } = host.recv() else {
        panic!()
    };

    // Disconnecting while waiting cancels the request for the host.
    hub.disconnect(bob.conn);
    assert_eq!(
        host.recv(),
        ServerMessage::JoinRequestCancelled { request_id }
    );
    assert_eq!(hub.stats().waiting, 0);
}

#[test]
fn only_the_host_admits() {
    let hub = hub();
    let mut host = client(&hub);
    host.send(&hub, create_msg(None, true, false));
    let (code, _) = created(&mut host);
    let mut bob = client(&hub);
    bob.send(&hub, join_msg(&code, "Bob", None));
    bob.recv();
    let ServerMessage::JoinRequest { request_id, .. } = host.recv() else {
        panic!()
    };
    host.send(&hub, ClientMessage::Admit { request_id });
    host.recv();
    bob.recv();

    let mut carol = client(&hub);
    carol.send(&hub, join_msg(&code, "Carol", None));
    carol.recv();
    let ServerMessage::JoinRequest { request_id, .. } = host.recv() else {
        panic!()
    };
    bob.send(&hub, ClientMessage::Admit { request_id });
    expect_error(&mut bob, ErrorCode::NotHost);
}

#[test]
fn ending_a_room_releases_lobby_waiters() {
    let hub = hub();
    let mut host = client(&hub);
    host.send(&hub, create_msg(None, true, false));
    let (code, _) = created(&mut host);
    let mut bob = client(&hub);
    bob.send(&hub, join_msg(&code, "Bob", None));
    bob.recv();
    host.send(&hub, ClientMessage::EndRoom);
    assert_eq!(
        bob.recv(),
        ServerMessage::RoomEnded {
            reason: RoomEndReason::HostEnded
        }
    );
    // Bob can create his own room afterwards.
    bob.send(&hub, create_msg(None, false, false));
    created(&mut bob);
}

#[test]
fn pins_are_checked_and_locked_out() {
    let hub = hub_with(HubConfig {
        max_wrong_pins: 3,
        ..Default::default()
    });
    let mut host = client(&hub);
    host.send(&hub, create_msg(Some("12"), false, false));
    expect_error(&mut host, ErrorCode::InvalidPin);
    host.send(&hub, create_msg(Some("4321"), false, false));
    let code = match host.recv() {
        ServerMessage::RoomCreated {
            room_id, security, ..
        } => {
            assert!(security.pin && !security.lobby);
            room_id
        }
        other => panic!("unexpected {other:?}"),
    };

    let mut bob = client_from(&hub, "10.0.0.2");
    bob.send(&hub, join_msg(&code, "Bob", None));
    expect_error(&mut bob, ErrorCode::PinRequired);
    for _ in 0..3 {
        bob.send(&hub, join_msg(&code, "Bob", Some("0000")));
        expect_error(&mut bob, ErrorCode::WrongPin);
    }
    // Locked: even the right PIN is refused for now.
    bob.send(&hub, join_msg(&code, "Bob", Some("4321")));
    expect_error(&mut bob, ErrorCode::RateLimited);
    // The lockout lifts after it expires.
    hub.sweep(Instant::now() + Duration::from_secs(301));
    bob.send(&hub, join_msg(&code, "Bob", Some("4321")));
    assert!(matches!(bob.recv(), ServerMessage::RoomJoined { .. }));
}

#[test]
fn device_proofs_attach_verified_ids() {
    let (hub, mut events) = events_hub(HubConfig::default());
    let mut host = client(&hub);
    let host_dev = verify_device(&hub, &mut host, 1);
    assert!(matches!(
        drain(&mut events).as_slice(),
        [HubEvent::DeviceSeen { .. }]
    ));
    host.send(&hub, create_msg(None, false, false));
    let (code, _) = created(&mut host);

    let mut bob = client(&hub);
    let bob_dev = verify_device(&hub, &mut bob, 2);
    bob.send(&hub, join_msg(&code, "Bob", None));
    match bob.recv() {
        ServerMessage::RoomJoined { participants, .. } => {
            assert_eq!(
                participants[0].device_id.as_deref(),
                Some(host_dev.as_str())
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    match host.recv() {
        ServerMessage::ParticipantJoined { participant } => {
            assert_eq!(participant.device_id.as_deref(), Some(bob_dev.as_str()));
        }
        other => panic!("unexpected {other:?}"),
    }
    let kinds: Vec<String> = drain(&mut events)
        .into_iter()
        .filter_map(|e| match e {
            HubEvent::Audit(a) => Some(a.kind),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["room_created", "participant_joined"]);
}

#[test]
fn bad_device_proofs_are_rejected() {
    let hub = hub();
    let mut c = client(&hub);
    c.send(
        &hub,
        ClientMessage::DeviceProof {
            signature: "AAAA".into(),
        },
    );
    expect_error(&mut c, ErrorCode::NotVerified);
    c.send(
        &hub,
        ClientMessage::DeviceHello {
            public_key: "bm90IGEga2V5".into(),
            name: None,
            platform: None,
        },
    );
    expect_error(&mut c, ErrorCode::NotVerified);
}

#[test]
fn trusted_devices_skip_the_lobby() {
    let hub = hub();
    let mut host = client(&hub);
    let host_dev = verify_device(&hub, &mut host, 1);
    host.send(&hub, create_msg(None, true, false));
    let (code, _) = created(&mut host);

    let mut bob = client(&hub);
    let bob_dev = verify_device(&hub, &mut bob, 2);
    hub.load_trust(&host_dev, [bob_dev.clone()]);
    bob.send(&hub, join_msg(&code, "Bob", None));
    assert!(matches!(bob.recv(), ServerMessage::RoomJoined { .. }));

    // An untrusted device still waits.
    let mut eve = client(&hub);
    verify_device(&hub, &mut eve, 3);
    eve.send(&hub, join_msg(&code, "Eve", None));
    assert!(matches!(eve.recv(), ServerMessage::LobbyWaiting { .. }));
}

#[test]
fn report_event_whitelist() {
    let (hub, mut events) = events_hub(HubConfig::default());
    let mut host = client(&hub);
    verify_device(&hub, &mut host, 1);
    host.send(&hub, create_msg(None, false, false));
    created(&mut host);
    drain(&mut events);
    host.send(
        &hub,
        ClientMessage::ReportEvent {
            kind: "control_granted".into(),
            subject: None,
        },
    );
    assert!(
        matches!(drain(&mut events).as_slice(), [HubEvent::Audit(a)] if a.kind == "control_granted")
    );
    host.send(
        &hub,
        ClientMessage::ReportEvent {
            kind: "drop_tables".into(),
            subject: None,
        },
    );
    expect_error(&mut host, ErrorCode::InvalidMessage);
}

#[test]
fn large_rooms_need_an_sfu() {
    let hub = hub();
    let mut host = client(&hub);
    host.send(&hub, create_msg(None, false, true));
    expect_error(&mut host, ErrorCode::Unavailable);

    let (hub, mut events) = events_hub(HubConfig {
        sfu_available: true,
        sfu_max_participants: 25,
        ..Default::default()
    });
    let mut host = client(&hub);
    host.send(&hub, create_msg(None, false, true));
    let code = match host.recv() {
        ServerMessage::RoomCreated {
            room_id,
            topology,
            max_participants,
            ..
        } => {
            assert_eq!(topology, Topology::Sfu);
            assert_eq!(max_participants, 25);
            room_id
        }
        other => panic!("unexpected {other:?}"),
    };
    host.send(&hub, ClientMessage::SfuOffer { sdp: "v=0".into() });
    assert!(matches!(
        drain(&mut events).as_slice(),
        [HubEvent::Sfu { room, signal: SfuSignal::Offer(_), .. }] if *room == code
    ));
    assert_eq!(hub.stats().sfu_rooms, 1);
}

#[test]
fn sfu_signals_are_refused_in_mesh_rooms() {
    let (hub, _events) = events_hub(HubConfig::default());
    let (mut a, ..) = create(&hub);
    a.send(&hub, ClientMessage::SfuOffer { sdp: "v=0".into() });
    expect_error(&mut a, ErrorCode::Unavailable);
}

#[test]
fn cluster_prefix_is_applied() {
    let hub = hub_with(HubConfig {
        code_prefix: Some(7),
        ..Default::default()
    });
    let (_a, code, ..) = create(&hub);
    assert!(code.starts_with('7'));
    assert!(hub.has_room(&code));
}

#[test]
fn metrics_count_activity() {
    let hub = hub();
    let (_a, code, ..) = create(&hub);
    let _b = join(&hub, &code, "B");
    let mut c = client(&hub);
    c.send(&hub, join_msg("123456789", "C", None));
    expect_error(&mut c, ErrorCode::RoomNotFound);
    let m = hub.metrics();
    let get = |name: &str| m.counters.iter().find(|c| c.0 == name).unwrap().2;
    assert_eq!(get("connexa_rooms_created_total"), 1);
    assert_eq!(get("connexa_joins_total"), 1);
    assert_eq!(m.errors, vec![("room_not_found".to_string(), 1)]);
}
