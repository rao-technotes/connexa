//! Selective forwarding unit for large Connexa rooms.
//!
//! Each participant has two peer connections with the SFU:
//!
//! - **publisher**: the client offers, the SFU answers. Carries the
//!   participant's mic, camera and screen upstream, plus the `connexa` data
//!   channel for peer messages (`{to?, msg}`).
//! - **subscriber**: the SFU offers, the client answers. Carries everyone
//!   else's tracks downstream, plus a `connexa` data channel delivering peer
//!   messages (`{from, msg}`).
//!
//! Each side is the offerer on exactly one connection, so renegotiation never
//! collides. Media is forwarded packet by packet (no transcoding); keyframe
//! requests from subscribers are passed to the publisher. Extra data channels
//! opened by a client as `relay:<target>:<label>` are piped to the target as
//! `from:<sender>:<label>` (used for file transfer).
//!
//! All ICE traffic shares one UDP port. Behind 1:1 NAT (cloud VMs,
//! Kubernetes) set the public IP so it is advertised in candidates.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bytes::Bytes;
use connexa_protocol::{ServerMessage, SfuPc};
use serde_json::{Value, json};
use tokio::sync::{Mutex, watch};
use tracing::{debug, info, warn};
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MIME_TYPE_OPUS, MIME_TYPE_VP8, MediaEngine};
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::{API, APIBuilder};
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::dtls_transport::dtls_role::DTLSRole;
use webrtc::ice::network_type::NetworkType;
use webrtc::ice::udp_mux::{UDPMuxDefault, UDPMuxParams};
use webrtc::ice::udp_network::UDPNetwork;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_candidate_type::RTCIceCandidateType;
pub use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};
use webrtc::rtp_transceiver::rtp_sender::RTCRtpSender;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;
use webrtc::track::track_local::{TrackLocal, TrackLocalWriter};
use webrtc::track::track_remote::TrackRemote;

/// Sends a signaling message to one participant: `(room, participant, message)`.
pub type Outbox = Arc<dyn Fn(&str, &str, ServerMessage) + Send + Sync>;

/// Minimum spacing between keyframe requests forwarded for one track.
const PLI_MIN_INTERVAL: Duration = Duration::from_millis(500);
/// Largest peer message relayed through the SFU.
const MAX_RELAY_MESSAGE: usize = 256 * 1024;

#[derive(Debug, Clone)]
pub struct SfuConfig {
    pub udp_port: u16,
    pub public_ip: Option<IpAddr>,
    pub ice_servers: Vec<RTCIceServer>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SfuStats {
    pub rooms: u64,
    pub peers: u64,
    pub tracks: u64,
    pub packets_forwarded: u64,
}

pub struct Sfu {
    api: API,
    config: SfuConfig,
    rooms: Mutex<HashMap<String, Arc<Room>>>,
    outbox: Outbox,
    packets: Arc<AtomicU64>,
}

struct Room {
    id: String,
    peers: Mutex<HashMap<String, Arc<Peer>>>,
    tracks: Mutex<Vec<Arc<Forwarded>>>,
    /// Messages addressed to participants who haven't connected to the SFU
    /// yet (they joined the room but their first offer is still on its way).
    early: Mutex<HashMap<String, Vec<String>>>,
}

/// Early messages kept per not-yet-connected participant.
const MAX_EARLY_MESSAGES: usize = 256;

struct Peer {
    id: String,
    publisher: Arc<RTCPeerConnection>,
    subscriber: Arc<RTCPeerConnection>,
    /// Downstream `connexa` channel (SFU → client).
    downlink: Arc<Buffered>,
    negotiation: Mutex<Negotiation>,
    /// Relay channels opened towards this peer, by label.
    relays: Mutex<HashMap<String, Arc<Buffered>>>,
}

enum Payload {
    Text(String),
    Binary(Bytes),
}

/// A data channel that queues messages until it opens, preserving order.
struct Buffered {
    dc: Arc<RTCDataChannel>,
    pending: Mutex<Vec<Payload>>,
}

impl Buffered {
    fn new(dc: Arc<RTCDataChannel>) -> Arc<Self> {
        let me = Arc::new(Self {
            dc: dc.clone(),
            pending: Mutex::new(Vec::new()),
        });
        let weak = Arc::downgrade(&me);
        dc.on_open(Box::new(move || {
            let weak = weak.clone();
            Box::pin(async move {
                if let Some(me) = weak.upgrade() {
                    let mut pending = me.pending.lock().await;
                    for p in pending.drain(..) {
                        me.write(p).await;
                    }
                }
            })
        }));
        me
    }

    async fn send(&self, payload: Payload) {
        let mut pending = self.pending.lock().await;
        if self.dc.ready_state() == RTCDataChannelState::Open {
            for p in pending.drain(..) {
                self.write(p).await;
            }
            self.write(payload).await;
        } else {
            pending.push(payload);
        }
    }

    async fn write(&self, payload: Payload) {
        let _ = match payload {
            Payload::Text(t) => self.dc.send_text(t).await,
            Payload::Binary(b) => self.dc.send(&b).await,
        };
    }
}

#[derive(Default)]
struct Negotiation {
    in_flight: bool,
    again: bool,
}

/// A publisher's track being forwarded to every other participant.
struct Forwarded {
    owner: String,
    remote_id: String,
    kind: RTPCodecType,
    ssrc: u32,
    local: Arc<TrackLocalStaticRTP>,
    senders: Mutex<HashMap<String, Arc<RTCRtpSender>>>,
    stop: watch::Sender<bool>,
    last_pli: Mutex<Instant>,
    publisher: Weak<RTCPeerConnection>,
}

impl Forwarded {
    async fn request_keyframe(&self) {
        if self.kind != RTPCodecType::Video {
            return;
        }
        {
            let mut last = self.last_pli.lock().await;
            if last.elapsed() < PLI_MIN_INTERVAL {
                return;
            }
            *last = Instant::now();
        }
        if let Some(publisher) = self.publisher.upgrade() {
            let pli = PictureLossIndication {
                sender_ssrc: 0,
                media_ssrc: self.ssrc,
            };
            let _ = publisher.write_rtcp(&[Box::new(pli)]).await;
        }
    }
}

impl Sfu {
    /// Bind the media UDP port and build the WebRTC stack. Call inside a Tokio runtime.
    pub fn new(config: SfuConfig, outbox: Outbox) -> Result<Arc<Self>> {
        let mut media = MediaEngine::default();
        // One codec per kind that every browser supports, so forwarded
        // packets never need transcoding.
        media.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_VP8.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: String::new(),
                    rtcp_feedback: vec![],
                },
                payload_type: 96,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
        media.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48000,
                    channels: 2,
                    sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 111,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )?;
        let registry = register_default_interceptors(Registry::new(), &mut media)?;

        let socket = std::net::UdpSocket::bind(("0.0.0.0", config.udp_port))
            .with_context(|| format!("binding SFU UDP port {}", config.udp_port))?;
        socket.set_nonblocking(true)?;
        disable_udp_connreset(&socket);
        let socket = tokio::net::UdpSocket::from_std(socket)?;
        let mut settings = SettingEngine::default();
        // IPv4 only: keeps candidate lists short, and IPv6 interface enumeration
        // is unreliable on some platforms.
        settings.set_network_types(vec![NetworkType::Udp4]);
        // Always act as the DTLS client (see `force_dtls_client`).
        settings.set_answering_dtls_role(DTLSRole::Client)?;
        settings.set_udp_network(UDPNetwork::Muxed(UDPMuxDefault::new(UDPMuxParams::new(
            socket,
        ))));
        if let Some(ip) = config.public_ip {
            settings.set_nat_1to1_ips(vec![ip.to_string()], RTCIceCandidateType::Host);
        }

        let api = APIBuilder::new()
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_setting_engine(settings)
            .build();
        info!(udp_port = config.udp_port, "SFU ready");
        Ok(Arc::new(Self {
            api,
            config,
            rooms: Mutex::new(HashMap::new()),
            outbox,
            packets: Arc::new(AtomicU64::new(0)),
        }))
    }

    pub async fn stats(&self) -> SfuStats {
        let rooms = self.rooms.lock().await;
        let mut stats = SfuStats {
            rooms: rooms.len() as u64,
            packets_forwarded: self.packets.load(Ordering::Relaxed),
            ..Default::default()
        };
        for room in rooms.values() {
            stats.peers += room.peers.lock().await.len() as u64;
            stats.tracks += room.tracks.lock().await.len() as u64;
        }
        stats
    }

    /// Client offer for its publisher connection.
    pub async fn publisher_offer(self: &Arc<Self>, room: &str, participant: &str, sdp: String) {
        if let Err(e) = self.try_publisher_offer(room, participant, sdp).await {
            warn!(participant, "SFU publisher negotiation failed: {e:#}");
        }
    }

    /// Client answer for its subscriber connection.
    pub async fn subscriber_answer(self: &Arc<Self>, room: &str, participant: &str, sdp: String) {
        let Some(peer) = self.peer(room, participant).await else {
            return;
        };
        let result = async {
            let answer = RTCSessionDescription::answer(sdp)?;
            peer.subscriber.set_remote_description(answer).await?;
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = result {
            warn!(participant, "SFU subscriber answer rejected: {e:#}");
        }
        let again = {
            let mut n = peer.negotiation.lock().await;
            n.in_flight = false;
            std::mem::take(&mut n.again)
        };
        if again {
            self.negotiate_subscriber(room, &peer).await;
        }
    }

    pub async fn candidate(&self, room: &str, participant: &str, pc: SfuPc, candidate: Value) {
        let Some(peer) = self.peer(room, participant).await else {
            return;
        };
        let init: RTCIceCandidateInit = match serde_json::from_value(candidate) {
            Ok(c) => c,
            Err(_) => return,
        };
        if init.candidate.is_empty() {
            return; // end-of-candidates
        }
        let target = match pc {
            SfuPc::Publisher => &peer.publisher,
            SfuPc::Subscriber => &peer.subscriber,
        };
        if let Err(e) = target.add_ice_candidate(init).await {
            debug!(participant, "ignoring ICE candidate: {e}");
        }
    }

    pub async fn leave(&self, room_id: &str, participant: &str) {
        let Some(room) = self.rooms.lock().await.get(room_id).cloned() else {
            return;
        };
        room.early.lock().await.remove(participant);
        let Some(peer) = room.peers.lock().await.remove(participant) else {
            return;
        };
        // Stop forwarding their tracks and drop them from everyone else.
        let owned: Vec<Arc<Forwarded>> = {
            let mut tracks = room.tracks.lock().await;
            let (mine, rest): (Vec<_>, Vec<_>) =
                tracks.drain(..).partition(|t| t.owner == participant);
            *tracks = rest;
            mine
        };
        for t in &owned {
            self.unpublish(&room, t).await;
        }
        // And stop sending other tracks to them.
        for t in room.tracks.lock().await.iter() {
            t.senders.lock().await.remove(participant);
        }
        let _ = peer.publisher.close().await;
        let _ = peer.subscriber.close().await;
        debug!(participant, "left SFU");
        if room.peers.lock().await.is_empty() {
            self.rooms.lock().await.remove(room_id);
        }
    }

    pub async fn close_room(&self, room_id: &str) {
        let Some(room) = self.rooms.lock().await.remove(room_id) else {
            return;
        };
        for t in room.tracks.lock().await.drain(..) {
            let _ = t.stop.send(true);
        }
        for (_, peer) in room.peers.lock().await.drain() {
            let _ = peer.publisher.close().await;
            let _ = peer.subscriber.close().await;
        }
    }

    // ----- internals ---------------------------------------------------------------

    async fn room(&self, id: &str) -> Arc<Room> {
        self.rooms
            .lock()
            .await
            .entry(id.to_string())
            .or_insert_with(|| {
                Arc::new(Room {
                    id: id.to_string(),
                    peers: Mutex::new(HashMap::new()),
                    tracks: Mutex::new(Vec::new()),
                    early: Mutex::new(HashMap::new()),
                })
            })
            .clone()
    }

    async fn peer(&self, room: &str, participant: &str) -> Option<Arc<Peer>> {
        let room = self.rooms.lock().await.get(room).cloned()?;
        room.peers.lock().await.get(participant).cloned()
    }

    fn send(&self, room: &str, participant: &str, msg: ServerMessage) {
        (self.outbox)(room, participant, msg);
    }

    async fn try_publisher_offer(
        self: &Arc<Self>,
        room_id: &str,
        participant: &str,
        sdp: String,
    ) -> Result<()> {
        let room = self.room(room_id).await;
        let existing = room.peers.lock().await.get(participant).cloned();
        let (peer, is_new) = match existing {
            Some(p) => (p, false),
            None => (self.create_peer(&room, participant).await?, true),
        };

        peer.publisher
            .set_remote_description(RTCSessionDescription::offer(sdp)?)
            .await?;
        let answer = peer.publisher.create_answer(None).await?;
        peer.publisher.set_local_description(answer.clone()).await?;
        self.send(
            room_id,
            participant,
            ServerMessage::SfuAnswer { sdp: answer.sdp },
        );

        // Tracks the client stopped sending (transceiver now inactive) are unpublished.
        let mut still_sent = Vec::new();
        for t in peer.publisher.get_transceivers().await {
            if matches!(
                t.current_direction(),
                RTCRtpTransceiverDirection::Recvonly | RTCRtpTransceiverDirection::Sendrecv
            ) || t.current_direction() == RTCRtpTransceiverDirection::Unspecified
            {
                for track in t.receiver().await.tracks().await {
                    still_sent.push(track.id());
                }
            }
        }
        let stale: Vec<Arc<Forwarded>> = {
            let mut tracks = room.tracks.lock().await;
            let (gone, keep): (Vec<_>, Vec<_>) = tracks
                .drain(..)
                .partition(|t| t.owner == participant && !still_sent.contains(&t.remote_id));
            *tracks = keep;
            gone
        };
        for t in &stale {
            self.unpublish(&room, t).await;
        }

        if is_new {
            self.negotiate_subscriber(room_id, &peer).await;
        }
        Ok(())
    }

    async fn create_peer(
        self: &Arc<Self>,
        room: &Arc<Room>,
        participant: &str,
    ) -> Result<Arc<Peer>> {
        let config = RTCConfiguration {
            ice_servers: self.config.ice_servers.clone(),
            ..Default::default()
        };
        let publisher = Arc::new(self.api.new_peer_connection(config.clone()).await?);
        let subscriber = Arc::new(self.api.new_peer_connection(config).await?);
        let downlink = Buffered::new(subscriber.create_data_channel("connexa", None).await?);

        let peer = Arc::new(Peer {
            id: participant.to_string(),
            publisher: publisher.clone(),
            subscriber: subscriber.clone(),
            downlink,
            negotiation: Mutex::new(Negotiation::default()),
            relays: Mutex::new(HashMap::new()),
        });

        // Trickle ICE for both connections.
        for (pc, which) in [
            (&publisher, SfuPc::Publisher),
            (&subscriber, SfuPc::Subscriber),
        ] {
            let sfu = Arc::downgrade(self);
            let room_id = room.id.clone();
            let pid = participant.to_string();
            pc.on_ice_candidate(Box::new(move |c| {
                let sfu = sfu.clone();
                let room_id = room_id.clone();
                let pid = pid.clone();
                Box::pin(async move {
                    let (Some(sfu), Some(c)) = (sfu.upgrade(), c) else {
                        return;
                    };
                    if let Ok(init) = c.to_json() {
                        sfu.send(
                            &room_id,
                            &pid,
                            ServerMessage::SfuCandidate {
                                pc: which,
                                candidate: serde_json::to_value(init).unwrap_or(Value::Null),
                            },
                        );
                    }
                })
            }));
        }

        // Upstream media → forward to everyone else.
        {
            let sfu = Arc::downgrade(self);
            let room_w = Arc::downgrade(room);
            let pid = participant.to_string();
            let publisher_w = Arc::downgrade(&publisher);
            publisher.on_track(Box::new(move |track, _receiver, _transceiver| {
                let (sfu, room_w, pid, publisher_w) = (
                    sfu.clone(),
                    room_w.clone(),
                    pid.clone(),
                    publisher_w.clone(),
                );
                Box::pin(async move {
                    if let (Some(sfu), Some(room)) = (sfu.upgrade(), room_w.upgrade()) {
                        sfu.publish(&room, &pid, track, publisher_w).await;
                    }
                })
            }));
        }

        // Upstream data channels: `connexa` (peer messages) and `relay:*` (files).
        {
            let room_w = Arc::downgrade(room);
            let pid = participant.to_string();
            let sfu = Arc::downgrade(self);
            publisher.on_data_channel(Box::new(move |dc| {
                let (room_w, pid, sfu) = (room_w.clone(), pid.clone(), sfu.clone());
                Box::pin(async move {
                    let (Some(room), Some(sfu)) = (room_w.upgrade(), sfu.upgrade()) else {
                        return;
                    };
                    let label = dc.label().to_string();
                    if label == "connexa" {
                        sfu.wire_uplink(&room, &pid, dc);
                    } else if let Some(rest) = label.strip_prefix("relay:")
                        && let Some((target, inner)) = rest.split_once(':')
                    {
                        sfu.wire_relay(&room, &pid, target, inner, dc).await;
                    }
                })
            }));
        }

        // Send everything already being published to the newcomer.
        for t in room.tracks.lock().await.iter() {
            if t.owner != participant {
                self.add_sender(t, &peer).await;
            }
        }
        room.peers
            .lock()
            .await
            .insert(participant.to_string(), peer.clone());
        let early = room.early.lock().await.remove(participant);
        for msg in early.into_iter().flatten() {
            peer.downlink.send(Payload::Text(msg)).await;
        }
        let size = room.peers.lock().await.len();
        debug!(participant, size, "joined SFU");
        Ok(peer)
    }

    async fn publish(
        self: &Arc<Self>,
        room: &Arc<Room>,
        owner: &str,
        remote: Arc<TrackRemote>,
        publisher: Weak<RTCPeerConnection>,
    ) {
        // Stream ids carry the owner so subscribers can attribute tracks.
        let stream_id = format!("{owner}~{}", remote.stream_id());
        let local = Arc::new(TrackLocalStaticRTP::new(
            remote.codec().capability,
            remote.id(),
            stream_id,
        ));
        let (stop, mut stopped) = watch::channel(false);
        let fwd = Arc::new(Forwarded {
            owner: owner.to_string(),
            remote_id: remote.id(),
            kind: remote.kind(),
            ssrc: remote.ssrc(),
            local: local.clone(),
            senders: Mutex::new(HashMap::new()),
            stop,
            last_pli: Mutex::new(Instant::now() - PLI_MIN_INTERVAL),
            publisher,
        });
        room.tracks.lock().await.push(fwd.clone());
        let peers: Vec<Arc<Peer>> = room.peers.lock().await.values().cloned().collect();
        for peer in &peers {
            if peer.id != owner {
                self.add_sender(&fwd, peer).await;
                self.negotiate_subscriber(&room.id, peer).await;
            }
        }
        info!(owner, kind = %remote.kind(), "forwarding track");

        // Copy packets until the track ends or is unpublished.
        let packets = self.packets.clone();
        let sfu = Arc::downgrade(self);
        let room_w = Arc::downgrade(room);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stopped.changed() => break,
                    read = remote.read_rtp() => match read {
                        Ok((packet, _)) => {
                            if local.write_rtp(&packet).await.is_ok() {
                                packets.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Err(_) => break,
                    },
                }
            }
            // Ended on its own (publisher gone): clean up.
            if let (Some(sfu), Some(room)) = (sfu.upgrade(), room_w.upgrade()) {
                let still_listed = {
                    let mut tracks = room.tracks.lock().await;
                    let before = tracks.len();
                    tracks.retain(|t| !Arc::ptr_eq(t, &fwd));
                    tracks.len() != before
                };
                if still_listed {
                    sfu.unpublish(&room, &fwd).await;
                }
            }
        });
    }

    /// Stop forwarding `track` and remove it from every subscriber.
    async fn unpublish(&self, room: &Arc<Room>, track: &Arc<Forwarded>) {
        let _ = track.stop.send(true);
        let senders: Vec<(String, Arc<RTCRtpSender>)> =
            track.senders.lock().await.drain().collect();
        for (pid, sender) in senders {
            let peer = room.peers.lock().await.get(&pid).cloned();
            if let Some(peer) = peer {
                let _ = peer.subscriber.remove_track(&sender).await;
                self.negotiate_subscriber(&room.id, &peer).await;
            }
        }
    }

    async fn add_sender(&self, track: &Arc<Forwarded>, peer: &Arc<Peer>) {
        let local: Arc<dyn TrackLocal + Send + Sync> = track.local.clone();
        match peer.subscriber.add_track(local).await {
            Ok(sender) => {
                track
                    .senders
                    .lock()
                    .await
                    .insert(peer.id.clone(), sender.clone());
                // Read RTCP from the subscriber; forward keyframe requests upstream.
                let weak = Arc::downgrade(track);
                tokio::spawn(async move {
                    while let Ok((packets, _)) = sender.read_rtcp().await {
                        let wants_keyframe = packets
                            .iter()
                            .any(|p| p.as_any().downcast_ref::<PictureLossIndication>().is_some());
                        if wants_keyframe && let Some(t) = weak.upgrade() {
                            t.request_keyframe().await;
                        }
                    }
                });
                track.request_keyframe().await;
            }
            Err(e) => warn!("could not forward track: {e}"),
        }
    }

    async fn negotiate_subscriber(&self, room: &str, peer: &Arc<Peer>) {
        {
            let mut n = peer.negotiation.lock().await;
            if n.in_flight {
                n.again = true;
                return;
            }
            n.in_flight = true;
        }
        let result = async {
            let offer = peer.subscriber.create_offer(None).await?;
            peer.subscriber.set_local_description(offer.clone()).await?;
            anyhow::Ok(force_dtls_client(&offer.sdp))
        }
        .await;
        match result {
            Ok(sdp) => self.send(room, &peer.id, ServerMessage::SfuOffer { sdp }),
            Err(e) => {
                warn!(participant = %peer.id, "subscriber offer failed: {e:#}");
                peer.negotiation.lock().await.in_flight = false;
            }
        }
    }

    /// `{to?, msg}` from a participant → `{from, msg}` to the target(s).
    fn wire_uplink(self: &Arc<Self>, room: &Arc<Room>, from: &str, dc: Arc<RTCDataChannel>) {
        let room_w = Arc::downgrade(room);
        let from = from.to_string();
        dc.on_message(Box::new(move |msg: DataChannelMessage| {
            let (room_w, from) = (room_w.clone(), from.clone());
            Box::pin(async move {
                let Some(room) = room_w.upgrade() else { return };
                if !msg.is_string || msg.data.len() > MAX_RELAY_MESSAGE {
                    return;
                }
                let Ok(envelope) = serde_json::from_slice::<Value>(&msg.data) else {
                    return;
                };
                let to = envelope.get("to").and_then(Value::as_str).map(String::from);
                let Some(inner) = envelope.get("msg").cloned() else {
                    return;
                };
                let out = json!({ "from": from, "msg": inner }).to_string();
                let targets: Vec<Arc<Peer>> = {
                    let peers = room.peers.lock().await;
                    if let Some(target) = to.as_deref()
                        && !peers.contains_key(target)
                    {
                        drop(peers);
                        let mut early = room.early.lock().await;
                        let queue = early.entry(target.to_string()).or_default();
                        if queue.len() < MAX_EARLY_MESSAGES {
                            queue.push(out);
                        }
                        return;
                    }
                    peers
                        .values()
                        .filter(|p| p.id != from && to.as_deref().is_none_or(|t| t == p.id))
                        .cloned()
                        .collect()
                };
                for peer in targets {
                    peer.downlink.send(Payload::Text(out.clone())).await;
                }
            })
        }));
    }

    /// Pipe a client-opened `relay:<target>:<label>` channel to the target.
    async fn wire_relay(
        self: &Arc<Self>,
        room: &Arc<Room>,
        from: &str,
        target: &str,
        label: &str,
        upstream: Arc<RTCDataChannel>,
    ) {
        let Some(peer) = room.peers.lock().await.get(target).cloned() else {
            let _ = upstream.close().await;
            return;
        };
        let down_label = format!("from:{from}:{label}");
        let downstream = match peer.subscriber.create_data_channel(&down_label, None).await {
            Ok(dc) => Buffered::new(dc),
            Err(e) => {
                warn!("relay channel failed: {e}");
                let _ = upstream.close().await;
                return;
            }
        };
        peer.relays
            .lock()
            .await
            .insert(down_label, downstream.clone());
        let down = downstream.clone();
        upstream.on_message(Box::new(move |msg: DataChannelMessage| {
            let down = down.clone();
            Box::pin(async move {
                let payload = if msg.is_string {
                    Payload::Text(String::from_utf8_lossy(&msg.data).into_owned())
                } else {
                    Payload::Binary(msg.data)
                };
                down.send(payload).await;
            })
        }));
        // The sender's close is deliberately not forwarded: data may still be
        // queued towards the receiver. The receiver closes its end once it has
        // everything (or on an explicit `file-cancel`), which closes ours.
        let up = upstream.clone();
        downstream.dc.on_close(Box::new(move || {
            let up = up.clone();
            Box::pin(async move {
                let _ = up.close().await;
            })
        }));
    }
}

/// On Windows, an ICMP "port unreachable" makes the next `recv_from` on a UDP
/// socket fail with WSAECONNRESET (10054), which would stop the shared media
/// socket. Turn that behaviour off, as UDP servers on Windows must.
#[cfg(windows)]
fn disable_udp_connreset(socket: &std::net::UdpSocket) {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{SIO_UDP_CONNRESET, WSAIoctl};
    let off: u32 = 0;
    let mut returned: u32 = 0;
    // SAFETY: valid socket handle and correctly sized in/out buffers.
    let rc = unsafe {
        WSAIoctl(
            socket.as_raw_socket() as usize,
            SIO_UDP_CONNRESET,
            &off as *const u32 as *const _,
            std::mem::size_of::<u32>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
            None,
        )
    };
    if rc != 0 {
        warn!("could not disable UDP connection-reset reporting");
    }
}

#[cfg(not(windows))]
fn disable_udp_connreset(_socket: &std::net::UdpSocket) {}

/// Offer `a=setup:active` so the browser answers `passive` and we stay the
/// DTLS client on every connection. Recent browsers offer key-exchange groups
/// (e.g. post-quantum hybrids) in their ClientHello that the DTLS server of
/// webrtc-rs rejects, while its client handshake interoperates. RFC 5763
/// allows an offerer to choose `active`.
fn force_dtls_client(sdp: &str) -> String {
    sdp.replace("a=setup:actpass", "a=setup:active")
}

#[cfg(test)]
mod tests {
    use super::force_dtls_client;

    #[test]
    fn offers_are_rewritten_to_active() {
        let sdp = "m=video 9
a=setup:actpass
m=application 9
a=setup:actpass
";
        let out = force_dtls_client(sdp);
        assert_eq!(out.matches("a=setup:active").count(), 2);
        assert!(!out.contains("actpass"));
    }
}
