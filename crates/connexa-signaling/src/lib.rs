//! Transport-agnostic room hub.
//!
//! The hub owns every room in memory and routes protocol messages between
//! participants. It knows nothing about WebSockets: a transport creates a
//! [`Connection`] per client, feeds decoded [`ClientMessage`]s into
//! [`Hub::handle`] and forwards whatever arrives on the connection's outbound
//! channel. That keeps it embeddable in the cloud server as well as in a
//! desktop app acting as a temporary LAN signaling server.
//!
//! Work that needs I/O (database writes, the SFU) is handed to the transport
//! as [`HubEvent`]s so the hub itself stays synchronous and lock-light.
//!
//! Lock order: `rooms` before `conns`. Never hold a `conns` guard while
//! touching `rooms`.

mod metrics;

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use connexa_core::DEFAULT_MAX_PARTICIPANTS;
use connexa_protocol::{
    ClientMessage, ErrorCode, IceServer, ParticipantId, ParticipantInfo, RoomEndReason,
    RoomSecurity, ServerMessage, SfuPc, Topology,
};
use connexa_security::{
    RateLimiter, constant_time_eq, device, generate_participant_id, generate_room_code_with_prefix,
    generate_token, normalize_room_code, redact_room_code, room_hash, sanitize_display_name,
    validate_pin,
};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

pub use metrics::{HubMetrics, MetricsSnapshot};

/// Channel carrying messages from the hub to one client connection.
pub type Outbound = mpsc::Sender<ServerMessage>;

/// Produces the ICE servers handed to a participant (lets the server mint
/// per-participant, time-limited TURN credentials).
pub type IceProvider = Arc<dyn Fn(&str) -> Vec<IceServer> + Send + Sync>;

/// Messages queued for a participant whose connection dropped, replayed on resume.
const MAX_PENDING_MESSAGES: usize = 512;
/// Joiners that may wait in one room's lobby at the same time.
const MAX_WAITING: usize = 10;

/// Peer-to-peer events clients may record in their audit log.
pub const REPORTABLE_EVENTS: &[&str] = &[
    "control_granted",
    "control_revoked",
    "file_sent",
    "file_received",
    "clipboard_shared",
];

#[derive(Debug, Clone)]
pub struct HubConfig {
    pub max_participants: usize,
    pub max_rooms: usize,
    /// A room with no signaling activity (pings included) for this long is closed.
    pub room_idle_timeout: Duration,
    /// Hard cap on a room's lifetime.
    pub room_max_lifetime: Duration,
    /// How long a dropped participant keeps its slot before being removed.
    pub reconnect_grace: Duration,
    /// Join / resume attempts allowed per IP per `rate_window` (guards code guessing).
    pub join_attempts_per_window: u32,
    /// Rooms that may be created per IP per `rate_window`.
    pub creates_per_window: u32,
    pub rate_window: Duration,
    /// Cluster node slot (1–9): the first digit of every room code this hub creates.
    pub code_prefix: Option<u8>,
    /// Whether an SFU is attached (enables `large` rooms).
    pub sfu_available: bool,
    pub sfu_max_participants: usize,
    /// Wrong PIN guesses per room before new joins are paused.
    pub max_wrong_pins: u32,
    pub pin_lockout: Duration,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            max_participants: DEFAULT_MAX_PARTICIPANTS,
            max_rooms: 10_000,
            room_idle_timeout: Duration::from_secs(30 * 60),
            room_max_lifetime: Duration::from_secs(12 * 60 * 60),
            reconnect_grace: Duration::from_secs(30),
            join_attempts_per_window: 20,
            creates_per_window: 10,
            rate_window: Duration::from_secs(60),
            code_prefix: None,
            sfu_available: false,
            sfu_max_participants: 25,
            max_wrong_pins: 5,
            pin_lockout: Duration::from_secs(5 * 60),
        }
    }
}

/// A verified device (proved possession of its private key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub platform: String,
}

/// Work for the async side of the server.
#[derive(Debug, Clone)]
pub enum HubEvent {
    DeviceSeen {
        device: DeviceInfo,
        public_key: String,
    },
    Audit(AuditRecord),
    /// SFU signaling from a participant of an SFU room.
    Sfu {
        room: String,
        participant: ParticipantId,
        signal: SfuSignal,
    },
    ParticipantLeft {
        room: String,
        participant: ParticipantId,
    },
    RoomClosed {
        room: String,
    },
}

#[derive(Debug, Clone)]
pub enum SfuSignal {
    Offer(String),
    Answer(String),
    Candidate(SfuPc, serde_json::Value),
}

#[derive(Debug, Clone)]
pub struct AuditRecord {
    pub at: SystemTime,
    pub kind: String,
    /// [`room_hash`] of the room code, never the code itself.
    pub room: String,
    pub actor: Option<String>,
    pub subject: Option<String>,
}

/// One client transport connection (e.g. one WebSocket).
pub struct Connection {
    id: u64,
    tx: Outbound,
    remote_ip: Option<IpAddr>,
}

impl Connection {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Send a message to this connection only (e.g. a transport-level error).
    pub fn send(&self, msg: ServerMessage) {
        if let Err(e) = self.tx.try_send(msg) {
            warn!(conn = self.id, "dropping reply: {e}");
        }
    }
}

#[derive(Clone, Debug)]
struct Session {
    room_id: String,
    participant_id: ParticipantId,
}

#[derive(Clone, Debug)]
enum Attach {
    None,
    Waiting { room_id: String, request_id: String },
    Joined(Session),
}

struct PendingDevice {
    public_key: String,
    id: String,
    name: String,
    platform: String,
    nonce: String,
}

struct ConnState {
    device: Option<DeviceInfo>,
    challenge: Option<PendingDevice>,
    attach: Attach,
}

struct Room {
    host_id: ParticipantId,
    participants: HashMap<ParticipantId, Participant>,
    waiting: HashMap<String, Waiting>,
    created_at: Instant,
    last_activity: Instant,
    pin: Option<String>,
    lobby: bool,
    topology: Topology,
    max_participants: usize,
    wrong_pins: u32,
    pin_locked_until: Option<Instant>,
}

struct Waiting {
    conn_id: u64,
    tx: Outbound,
    display_name: String,
    device: Option<DeviceInfo>,
}

struct Participant {
    id: ParticipantId,
    display_name: String,
    resume_token: String,
    conn_id: u64,
    tx: Option<Outbound>,
    disconnected_at: Option<Instant>,
    pending: VecDeque<ServerMessage>,
    device: Option<DeviceInfo>,
}

impl Participant {
    fn info(&self, host_id: &str) -> ParticipantInfo {
        ParticipantInfo {
            participant_id: self.id.clone(),
            display_name: self.display_name.clone(),
            is_host: self.id == host_id,
            device_id: self.device.as_ref().map(|d| d.id.clone()),
        }
    }

    /// Send now, or queue until the participant resumes.
    fn deliver(&mut self, msg: ServerMessage) {
        match &self.tx {
            Some(tx) => {
                if let Err(e) = tx.try_send(msg) {
                    warn!(participant = %self.id, "dropping message for slow or closed connection: {e}");
                }
            }
            None => {
                if self.pending.len() == MAX_PENDING_MESSAGES {
                    self.pending.pop_front();
                }
                self.pending.push_back(msg);
            }
        }
    }
}

impl Room {
    fn broadcast(&mut self, except: Option<&str>, msg: &ServerMessage) {
        for p in self.participants.values_mut() {
            if Some(p.id.as_str()) != except {
                p.deliver(msg.clone());
            }
        }
    }

    fn infos_except(&self, except: &str) -> Vec<ParticipantInfo> {
        self.participants
            .values()
            .filter(|p| p.id != except)
            .map(|p| p.info(&self.host_id))
            .collect()
    }

    fn security(&self) -> RoomSecurity {
        RoomSecurity {
            pin: self.pin.is_some(),
            lobby: self.lobby,
        }
    }

    fn host_device(&self) -> Option<&DeviceInfo> {
        self.participants
            .get(&self.host_id)
            .and_then(|p| p.device.as_ref())
    }

    fn device_of(&self, participant: &str) -> Option<&DeviceInfo> {
        self.participants
            .get(participant)
            .and_then(|p| p.device.as_ref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HubStats {
    pub rooms: usize,
    pub sfu_rooms: usize,
    pub participants: usize,
    pub waiting: usize,
    pub connections: usize,
}

pub struct Hub {
    config: HubConfig,
    ice: IceProvider,
    rooms: DashMap<String, Room>,
    conns: DashMap<u64, ConnState>,
    next_conn_id: AtomicU64,
    join_limiter: RateLimiter<IpAddr>,
    create_limiter: RateLimiter<IpAddr>,
    /// owner device → devices it trusts (auto-admitted through its lobbies).
    trust: DashMap<String, HashSet<String>>,
    events: Option<mpsc::UnboundedSender<HubEvent>>,
    metrics: HubMetrics,
}

impl Hub {
    pub fn new(config: HubConfig, ice: IceProvider) -> Self {
        Self {
            join_limiter: RateLimiter::new(config.join_attempts_per_window, config.rate_window),
            create_limiter: RateLimiter::new(config.creates_per_window, config.rate_window),
            config,
            ice,
            rooms: DashMap::new(),
            conns: DashMap::new(),
            next_conn_id: AtomicU64::new(1),
            trust: DashMap::new(),
            events: None,
            metrics: HubMetrics::default(),
        }
    }

    /// Deliver [`HubEvent`]s to the async side (database, SFU).
    pub fn with_events(mut self, events: mpsc::UnboundedSender<HubEvent>) -> Self {
        self.events = Some(events);
        self
    }

    pub fn config(&self) -> &HubConfig {
        &self.config
    }

    pub fn metrics(&self) -> MetricsSnapshot {
        self.metrics.snapshot()
    }

    pub fn connect(&self, tx: Outbound, remote_ip: Option<IpAddr>) -> Connection {
        self.connect_with_device(tx, remote_ip, None)
    }

    /// For connections whose device was already verified elsewhere (another cluster node).
    pub fn connect_with_device(
        &self,
        tx: Outbound,
        remote_ip: Option<IpAddr>,
        device: Option<DeviceInfo>,
    ) -> Connection {
        let id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
        self.conns.insert(
            id,
            ConnState {
                device,
                challenge: None,
                attach: Attach::None,
            },
        );
        self.metrics.inc(&self.metrics.connections_total);
        Connection { id, tx, remote_ip }
    }

    pub fn stats(&self) -> HubStats {
        let mut stats = HubStats {
            rooms: 0,
            sfu_rooms: 0,
            participants: 0,
            waiting: 0,
            connections: self.conns.len(),
        };
        for room in self.rooms.iter() {
            stats.rooms += 1;
            stats.participants += room.participants.len();
            stats.waiting += room.waiting.len();
            if room.topology == Topology::Sfu {
                stats.sfu_rooms += 1;
            }
        }
        stats
    }

    /// The verified device of a connection, if any.
    pub fn device_of(&self, conn: &Connection) -> Option<DeviceInfo> {
        self.conns.get(&conn.id).and_then(|c| c.device.clone())
    }

    /// Replace the cached trust list of `owner` (loaded from the database).
    pub fn load_trust(&self, owner: &str, devices: impl IntoIterator<Item = String>) {
        self.trust
            .insert(owner.to_string(), devices.into_iter().collect());
    }

    pub fn set_trust(&self, owner: &str, device: &str, trusted: bool) {
        let mut set = self.trust.entry(owner.to_string()).or_default();
        if trusted {
            set.insert(device.to_string());
        } else {
            set.remove(device);
        }
    }

    fn trusts(&self, owner: &str, device: &str) -> bool {
        self.trust.get(owner).is_some_and(|s| s.contains(device))
    }

    /// Whether a room with this code lives on this hub.
    pub fn has_room(&self, code: &str) -> bool {
        self.rooms.contains_key(code)
    }

    /// Send a message to one participant (used by the SFU for its signaling).
    pub fn send_to(&self, room: &str, participant: &str, msg: ServerMessage) -> bool {
        let Some(mut room) = self.rooms.get_mut(room) else {
            return false;
        };
        match room.participants.get_mut(participant) {
            Some(p) => {
                p.deliver(msg);
                true
            }
            None => false,
        }
    }

    pub fn is_participant(&self, room: &str, participant: &str) -> bool {
        self.rooms
            .get(room)
            .is_some_and(|r| r.participants.contains_key(participant))
    }

    pub fn handle(&self, conn: &mut Connection, msg: ClientMessage) {
        match msg {
            ClientMessage::CreateRoom {
                display_name,
                pin,
                lobby,
                large,
            } => self.create_room(conn, display_name, pin, lobby, large),
            ClientMessage::JoinRoom {
                room_id,
                display_name,
                pin,
            } => self.join_room(conn, &room_id, display_name, pin),
            ClientMessage::Resume {
                room_id,
                participant_id,
                resume_token,
            } => self.resume(conn, &room_id, &participant_id, &resume_token),
            ClientMessage::LeaveRoom => self.leave(conn),
            ClientMessage::EndRoom => self.end_room(conn),
            ClientMessage::Admit { request_id } => self.answer_join(conn, &request_id, true),
            ClientMessage::Deny { request_id } => self.answer_join(conn, &request_id, false),
            ClientMessage::SdpOffer { target, sdp } => {
                self.relay(conn, &target, |from| ServerMessage::SdpOffer { from, sdp })
            }
            ClientMessage::SdpAnswer { target, sdp } => {
                self.relay(conn, &target, |from| ServerMessage::SdpAnswer { from, sdp })
            }
            ClientMessage::IceCandidate { target, candidate } => {
                self.relay(conn, &target, |from| ServerMessage::IceCandidate {
                    from,
                    candidate,
                })
            }
            ClientMessage::SfuOffer { sdp } => self.sfu(conn, SfuSignal::Offer(sdp)),
            ClientMessage::SfuAnswer { sdp } => self.sfu(conn, SfuSignal::Answer(sdp)),
            ClientMessage::SfuCandidate { pc, candidate } => {
                self.sfu(conn, SfuSignal::Candidate(pc, candidate))
            }
            ClientMessage::DeviceHello {
                public_key,
                name,
                platform,
            } => self.device_hello(conn, public_key, name, platform),
            ClientMessage::DeviceProof { signature } => self.device_proof(conn, &signature),
            ClientMessage::ReportEvent { kind, subject } => self.report_event(conn, kind, subject),
            ClientMessage::TrustDevice { .. }
            | ClientMessage::ListTrustedDevices
            | ClientMessage::GetActivity => self.fail(
                conn,
                ErrorCode::Unavailable,
                "device management is not available on this server",
            ),
            ClientMessage::Ping => {
                if let Some(session) = self.current_session(conn)
                    && let Some(mut room) = self.rooms.get_mut(&session.room_id)
                {
                    room.last_activity = Instant::now();
                }
                conn.send(ServerMessage::Pong);
            }
        }
    }

    /// The transport closed. A participant keeps its slot for the reconnect
    /// grace period; [`Hub::sweep`] removes it afterwards. A lobby wait ends.
    pub fn disconnect(&self, conn: Connection) {
        let attach = self.conns.remove(&conn.id).map(|(_, c)| c.attach);
        match attach {
            Some(Attach::Joined(session)) => {
                if let Some(mut room) = self.rooms.get_mut(&session.room_id)
                    && let Some(p) = room.participants.get_mut(&session.participant_id)
                    && p.conn_id == conn.id
                {
                    debug!(participant = %p.id, "connection lost, holding slot for reconnect");
                    p.tx = None;
                    p.disconnected_at = Some(Instant::now());
                }
            }
            Some(Attach::Waiting {
                room_id,
                request_id,
            }) => self.cancel_wait(&room_id, &request_id),
            _ => {}
        }
    }

    /// Expire rooms and drop participants whose reconnect grace ran out.
    /// Call periodically (every few seconds).
    pub fn sweep(&self, now: Instant) {
        let cfg = &self.config;
        let mut closed = Vec::new();
        let mut left = Vec::new();
        self.rooms.retain(|code, room| {
            let idle = now.saturating_duration_since(room.last_activity) > cfg.room_idle_timeout;
            let too_old = now.saturating_duration_since(room.created_at) > cfg.room_max_lifetime;
            if idle || too_old {
                info!(room = %redact_room_code(code), "room expired");
                self.end_room_locked(room, RoomEndReason::Expired);
                closed.push(code.clone());
                return false;
            }

            let gone: Vec<ParticipantId> = room
                .participants
                .values()
                .filter(|p| {
                    p.disconnected_at
                        .is_some_and(|t| now.saturating_duration_since(t) >= cfg.reconnect_grace)
                })
                .map(|p| p.id.clone())
                .collect();
            for id in gone {
                room.participants.remove(&id);
                left.push((code.clone(), id.clone()));
                if id == room.host_id {
                    info!(room = %redact_room_code(code), "host did not reconnect, ending room");
                    self.end_room_locked(room, RoomEndReason::HostLeft);
                    closed.push(code.clone());
                    return false;
                }
                room.broadcast(None, &ServerMessage::ParticipantLeft { participant_id: id });
            }
            if room.pin_locked_until.is_some_and(|t| now >= t) {
                room.pin_locked_until = None;
            }
            true
        });
        for (room, participant) in left {
            self.emit(HubEvent::ParticipantLeft { room, participant });
        }
        for room in closed {
            self.emit(HubEvent::RoomClosed { room });
        }
        self.join_limiter.sweep(now);
        self.create_limiter.sweep(now);
    }

    // ----- rooms -------------------------------------------------------------

    fn create_room(
        &self,
        conn: &mut Connection,
        display_name: Option<String>,
        pin: Option<String>,
        lobby: bool,
        large: bool,
    ) {
        if !matches!(self.attach(conn), Attach::None) {
            return self.fail(
                conn,
                ErrorCode::AlreadyInRoom,
                "leave the current room first",
            );
        }
        if !allowed(&self.create_limiter, conn) {
            self.metrics.inc(&self.metrics.rate_limited_total);
            return self.fail(
                conn,
                ErrorCode::RateLimited,
                "too many rooms created, try again later",
            );
        }
        let pin = pin.filter(|p| !p.is_empty());
        if pin.as_deref().is_some_and(|p| !validate_pin(p)) {
            return self.fail(conn, ErrorCode::InvalidPin, "PINs are 4 to 8 digits");
        }
        if large && !self.config.sfu_available {
            return self.fail(
                conn,
                ErrorCode::Unavailable,
                "large meetings are not enabled on this server",
            );
        }
        if self.rooms.len() >= self.config.max_rooms {
            return self.fail(conn, ErrorCode::ServerBusy, "server is at capacity");
        }

        let device = self.conns.get(&conn.id).and_then(|c| c.device.clone());
        let (topology, max_participants) = if large {
            (Topology::Sfu, self.config.sfu_max_participants)
        } else {
            (Topology::Mesh, self.config.max_participants)
        };
        let now = Instant::now();
        let participant = Participant {
            id: generate_participant_id(),
            display_name: sanitize_display_name(display_name.as_deref(), "Host"),
            resume_token: generate_token(),
            conn_id: conn.id,
            tx: Some(conn.tx.clone()),
            disconnected_at: None,
            pending: VecDeque::new(),
            device: device.clone(),
        };

        for _ in 0..16 {
            let code = generate_room_code_with_prefix(self.config.code_prefix);
            let Entry::Vacant(slot) = self.rooms.entry(code.clone()) else {
                continue;
            };
            let room = Room {
                host_id: participant.id.clone(),
                participants: HashMap::new(),
                waiting: HashMap::new(),
                created_at: now,
                last_activity: now,
                pin: pin.clone(),
                lobby,
                topology,
                max_participants,
                wrong_pins: 0,
                pin_locked_until: None,
            };
            let msg = ServerMessage::RoomCreated {
                room_id: code.clone(),
                participant_id: participant.id.clone(),
                resume_token: participant.resume_token.clone(),
                max_participants,
                ice_servers: (self.ice)(&participant.id),
                topology,
                security: room.security(),
            };
            let host_id = participant.id.clone();
            let mut room = slot.insert(room);
            room.participants.insert(host_id.clone(), participant);
            drop(room);
            self.set_attach(
                conn.id,
                Attach::Joined(Session {
                    room_id: code.clone(),
                    participant_id: host_id,
                }),
            );
            self.metrics.inc(&self.metrics.rooms_created_total);
            info!(room = %redact_room_code(&code), ?topology, pin = pin.is_some(), lobby, "room created");
            self.audit("room_created", &code, device.as_ref(), None);
            return conn.send(msg);
        }
        self.fail(
            conn,
            ErrorCode::ServerBusy,
            "could not allocate a room code",
        );
    }

    fn join_room(
        &self,
        conn: &mut Connection,
        room_id: &str,
        display_name: Option<String>,
        pin: Option<String>,
    ) {
        if !matches!(self.attach(conn), Attach::None) {
            return self.fail(
                conn,
                ErrorCode::AlreadyInRoom,
                "leave the current room first",
            );
        }
        if !allowed(&self.join_limiter, conn) {
            self.metrics.inc(&self.metrics.rate_limited_total);
            return self.fail(
                conn,
                ErrorCode::RateLimited,
                "too many join attempts, try again later",
            );
        }
        let Some(code) = normalize_room_code(room_id) else {
            return self.fail(conn, ErrorCode::InvalidRoomCode, "room codes are 9 digits");
        };
        let device = self.conns.get(&conn.id).and_then(|c| c.device.clone());
        let Some(mut room) = self.rooms.get_mut(&code) else {
            return self.fail(
                conn,
                ErrorCode::RoomNotFound,
                "no active session with that code",
            );
        };

        if let Some(expected) = room.pin.clone() {
            if room.pin_locked_until.is_some_and(|t| Instant::now() < t) {
                drop(room);
                return self.fail(
                    conn,
                    ErrorCode::RateLimited,
                    "too many wrong PINs for this session, try again in a few minutes",
                );
            }
            match pin.as_deref() {
                None | Some("") => {
                    drop(room);
                    return self.fail(conn, ErrorCode::PinRequired, "this session needs a PIN");
                }
                Some(given) if !constant_time_eq(given, &expected) => {
                    room.wrong_pins += 1;
                    if room.wrong_pins >= self.config.max_wrong_pins {
                        room.wrong_pins = 0;
                        room.pin_locked_until = Some(Instant::now() + self.config.pin_lockout);
                        warn!(room = %redact_room_code(&code), "too many wrong PINs, pausing joins");
                    }
                    let host = room.host_device().cloned();
                    drop(room);
                    self.metrics.inc(&self.metrics.pin_failures_total);
                    self.audit("pin_failed", &code, host.as_ref(), device.as_ref());
                    return self.fail(conn, ErrorCode::WrongPin, "wrong PIN");
                }
                Some(_) => {}
            }
        }
        if room.participants.len() >= room.max_participants {
            drop(room);
            return self.fail(conn, ErrorCode::RoomFull, "this session is full");
        }
        let name = sanitize_display_name(
            display_name.as_deref(),
            &format!("Guest {}", room.participants.len()),
        );

        if room.lobby {
            let host = room.host_device().cloned();
            let trusted =
                matches!((&host, &device), (Some(h), Some(d)) if self.trusts(&h.id, &d.id));
            if !trusted {
                if room.waiting.len() >= MAX_WAITING {
                    drop(room);
                    return self.fail(
                        conn,
                        ErrorCode::ServerBusy,
                        "too many people are waiting to join",
                    );
                }
                let request_id = generate_token();
                room.waiting.insert(
                    request_id.clone(),
                    Waiting {
                        conn_id: conn.id,
                        tx: conn.tx.clone(),
                        display_name: name.clone(),
                        device: device.clone(),
                    },
                );
                let host_id = room.host_id.clone();
                if let Some(h) = room.participants.get_mut(&host_id) {
                    h.deliver(ServerMessage::JoinRequest {
                        request_id: request_id.clone(),
                        display_name: name,
                        device_id: device.as_ref().map(|d| d.id.clone()),
                    });
                }
                room.last_activity = Instant::now();
                // Set while still holding the room so an instant admit can't be overwritten.
                self.set_attach(
                    conn.id,
                    Attach::Waiting {
                        room_id: code.clone(),
                        request_id,
                    },
                );
                drop(room);
                self.metrics.inc(&self.metrics.lobby_requests_total);
                self.audit("join_requested", &code, device.as_ref(), None);
                return conn.send(ServerMessage::LobbyWaiting { room_id: code });
            }
            self.audit("auto_admitted", &code, host.as_ref(), device.as_ref());
        }

        self.admit_locked(&mut room, &code, conn.id, &conn.tx, name, device.clone());
        drop(room);
        self.audit("participant_joined", &code, device.as_ref(), None);
    }

    /// Add a participant to `room` (whose guard the caller holds).
    fn admit_locked(
        &self,
        room: &mut Room,
        code: &str,
        conn_id: u64,
        tx: &Outbound,
        name: String,
        device: Option<DeviceInfo>,
    ) {
        let participant = Participant {
            id: generate_participant_id(),
            display_name: name,
            resume_token: generate_token(),
            conn_id,
            tx: Some(tx.clone()),
            disconnected_at: None,
            pending: VecDeque::new(),
            device,
        };
        let msg = ServerMessage::RoomJoined {
            room_id: code.to_string(),
            participant_id: participant.id.clone(),
            resume_token: participant.resume_token.clone(),
            host_id: room.host_id.clone(),
            participants: room.infos_except(&participant.id),
            max_participants: room.max_participants,
            ice_servers: (self.ice)(&participant.id),
            topology: room.topology,
            security: room.security(),
        };
        room.broadcast(
            None,
            &ServerMessage::ParticipantJoined {
                participant: participant.info(&room.host_id),
            },
        );
        room.last_activity = Instant::now();
        let session = Session {
            room_id: code.to_string(),
            participant_id: participant.id.clone(),
        };
        room.participants
            .insert(participant.id.clone(), participant);
        self.set_attach(conn_id, Attach::Joined(session));
        self.metrics.inc(&self.metrics.joins_total);
        info!(room = %redact_room_code(code), size = room.participants.len(), "participant joined");
        if let Err(e) = tx.try_send(msg) {
            warn!(conn = conn_id, "dropping join reply: {e}");
        }
    }

    fn answer_join(&self, conn: &mut Connection, request_id: &str, admit: bool) {
        let Some(session) = self.current_session(conn) else {
            return self.fail(conn, ErrorCode::NotInRoom, "not in a room");
        };
        let Some(mut room) = self.rooms.get_mut(&session.room_id) else {
            return self.fail(conn, ErrorCode::NotInRoom, "room no longer exists");
        };
        if room.host_id != session.participant_id {
            drop(room);
            return self.fail(conn, ErrorCode::NotHost, "only the host can admit people");
        }
        let Some(waiting) = room.waiting.remove(request_id) else {
            drop(room);
            return self.fail(
                conn,
                ErrorCode::TargetNotFound,
                "that person is no longer waiting",
            );
        };
        let host = room.host_device().cloned();
        let code = session.room_id.clone();
        if admit && room.participants.len() < room.max_participants {
            let device = waiting.device.clone();
            self.admit_locked(
                &mut room,
                &code,
                waiting.conn_id,
                &waiting.tx,
                waiting.display_name,
                waiting.device,
            );
            drop(room);
            self.metrics.inc(&self.metrics.admitted_total);
            self.audit("admitted", &code, host.as_ref(), device.as_ref());
        } else {
            drop(room);
            let (code_err, text) = if admit {
                (ErrorCode::RoomFull, "this session is full")
            } else {
                (ErrorCode::JoinDenied, "the host did not let you in")
            };
            let _ = waiting.tx.try_send(ServerMessage::error(code_err, text));
            self.set_attach(waiting.conn_id, Attach::None);
            if !admit {
                self.metrics.inc(&self.metrics.denied_total);
                self.audit("denied", &code, host.as_ref(), waiting.device.as_ref());
            }
        }
    }

    fn cancel_wait(&self, room_id: &str, request_id: &str) {
        if let Some(mut room) = self.rooms.get_mut(room_id)
            && room.waiting.remove(request_id).is_some()
        {
            let host_id = room.host_id.clone();
            if let Some(h) = room.participants.get_mut(&host_id) {
                h.deliver(ServerMessage::JoinRequestCancelled {
                    request_id: request_id.to_string(),
                });
            }
        }
    }

    fn resume(&self, conn: &mut Connection, room_id: &str, participant_id: &str, token: &str) {
        if !matches!(self.attach(conn), Attach::None) {
            return self.fail(conn, ErrorCode::AlreadyInRoom, "already attached to a room");
        }
        if !allowed(&self.join_limiter, conn) {
            self.metrics.inc(&self.metrics.rate_limited_total);
            return self.fail(
                conn,
                ErrorCode::RateLimited,
                "too many attempts, try again later",
            );
        }
        let failed = |hub: &Hub, conn: &Connection| {
            hub.fail(
                conn,
                ErrorCode::ResumeFailed,
                "session can no longer be resumed",
            )
        };
        let Some(mut room) = self.rooms.get_mut(room_id) else {
            return failed(self, conn);
        };
        let host_id = room.host_id.clone();
        let participants = room.infos_except(participant_id);
        let Some(p) = room.participants.get_mut(participant_id) else {
            drop(room);
            return failed(self, conn);
        };
        if !constant_time_eq(&p.resume_token, token) {
            drop(room);
            return failed(self, conn);
        }

        p.conn_id = conn.id;
        p.tx = Some(conn.tx.clone());
        p.disconnected_at = None;
        p.deliver(ServerMessage::SessionResumed {
            room_id: room_id.to_string(),
            participant_id: participant_id.to_string(),
            host_id,
            participants,
        });
        while let Some(msg) = p.pending.pop_front() {
            p.deliver(msg);
        }
        room.last_activity = Instant::now();
        drop(room);
        self.set_attach(
            conn.id,
            Attach::Joined(Session {
                room_id: room_id.to_string(),
                participant_id: participant_id.to_string(),
            }),
        );
        self.metrics.inc(&self.metrics.resumes_total);
        debug!(participant = %participant_id, "session resumed");
    }

    fn leave(&self, conn: &mut Connection) {
        match self.attach(conn) {
            Attach::Waiting {
                room_id,
                request_id,
            } => {
                self.cancel_wait(&room_id, &request_id);
                self.set_attach(conn.id, Attach::None);
            }
            Attach::Joined(session) => {
                self.set_attach(conn.id, Attach::None);
                let host_left = {
                    let Some(mut room) = self.rooms.get_mut(&session.room_id) else {
                        return;
                    };
                    room.participants.remove(&session.participant_id);
                    if session.participant_id == room.host_id {
                        true
                    } else {
                        room.broadcast(
                            None,
                            &ServerMessage::ParticipantLeft {
                                participant_id: session.participant_id.clone(),
                            },
                        );
                        false
                    }
                };
                self.emit(HubEvent::ParticipantLeft {
                    room: session.room_id.clone(),
                    participant: session.participant_id.clone(),
                });
                if host_left {
                    self.close_room(&session.room_id, RoomEndReason::HostLeft);
                }
            }
            Attach::None => self.fail(conn, ErrorCode::NotInRoom, "not in a room"),
        }
    }

    fn end_room(&self, conn: &mut Connection) {
        let Some(session) = self.current_session(conn) else {
            return self.fail(conn, ErrorCode::NotInRoom, "not in a room");
        };
        let is_host = self
            .rooms
            .get(&session.room_id)
            .is_some_and(|r| r.host_id == session.participant_id);
        if !is_host {
            return self.fail(
                conn,
                ErrorCode::NotHost,
                "only the host can end the session",
            );
        }
        self.close_room(&session.room_id, RoomEndReason::HostEnded);
    }

    fn close_room(&self, room_id: &str, reason: RoomEndReason) {
        if let Some((code, mut room)) = self.rooms.remove(room_id) {
            info!(room = %redact_room_code(&code), ?reason, "room closed");
            self.end_room_locked(&mut room, reason);
            let host = room.host_device().cloned();
            self.audit("room_ended", &code, host.as_ref(), None);
            self.emit(HubEvent::RoomClosed { room: code });
        }
    }

    /// Tell everyone (including lobby waiters) the room is over.
    fn end_room_locked(&self, room: &mut Room, reason: RoomEndReason) {
        let msg = ServerMessage::RoomEnded { reason };
        room.broadcast(None, &msg);
        for (_, w) in room.waiting.drain() {
            let _ = w.tx.try_send(msg.clone());
            self.set_attach(w.conn_id, Attach::None);
        }
    }

    fn relay(
        &self,
        conn: &mut Connection,
        target: &str,
        make: impl FnOnce(ParticipantId) -> ServerMessage,
    ) {
        let Some(session) = self.current_session(conn) else {
            return self.fail(conn, ErrorCode::NotInRoom, "not in a room");
        };
        let Some(mut room) = self.rooms.get_mut(&session.room_id) else {
            return self.fail(conn, ErrorCode::NotInRoom, "room no longer exists");
        };
        room.last_activity = Instant::now();
        match room.participants.get_mut(target) {
            Some(peer) if peer.id != session.participant_id => {
                peer.deliver(make(session.participant_id));
                drop(room);
                self.metrics.inc(&self.metrics.relayed_total);
            }
            _ => {
                drop(room);
                self.fail(
                    conn,
                    ErrorCode::TargetNotFound,
                    "target is not in this room",
                )
            }
        }
    }

    fn sfu(&self, conn: &mut Connection, signal: SfuSignal) {
        let Some(session) = self.current_session(conn) else {
            return self.fail(conn, ErrorCode::NotInRoom, "not in a room");
        };
        let is_sfu = self.rooms.get_mut(&session.room_id).is_some_and(|mut r| {
            r.last_activity = Instant::now();
            r.topology == Topology::Sfu
        });
        if !is_sfu || self.events.is_none() {
            return self.fail(
                conn,
                ErrorCode::Unavailable,
                "this room does not use the SFU",
            );
        }
        self.emit(HubEvent::Sfu {
            room: session.room_id,
            participant: session.participant_id,
            signal,
        });
    }

    // ----- devices & audit -----------------------------------------------------

    fn device_hello(
        &self,
        conn: &mut Connection,
        public_key: String,
        name: Option<String>,
        platform: Option<String>,
    ) {
        let id = match device::device_id(&public_key) {
            Ok(id) => id,
            Err(_) => return self.fail(conn, ErrorCode::NotVerified, "invalid device key"),
        };
        let nonce = generate_token();
        if let Some(mut state) = self.conns.get_mut(&conn.id) {
            state.challenge = Some(PendingDevice {
                public_key,
                id,
                name: sanitize_display_name(name.as_deref(), "Device"),
                platform: sanitize_display_name(platform.as_deref(), "unknown")
                    .chars()
                    .take(16)
                    .collect(),
                nonce: nonce.clone(),
            });
        }
        conn.send(ServerMessage::DeviceChallenge { nonce });
    }

    fn device_proof(&self, conn: &mut Connection, signature: &str) {
        let pending = self
            .conns
            .get_mut(&conn.id)
            .and_then(|mut s| s.challenge.take());
        let Some(p) = pending else {
            return self.fail(conn, ErrorCode::NotVerified, "no device challenge pending");
        };
        if device::verify(&p.public_key, &p.nonce, signature).is_err() {
            return self.fail(conn, ErrorCode::NotVerified, "device proof failed");
        }
        let info = DeviceInfo {
            id: p.id.clone(),
            name: p.name,
            platform: p.platform,
        };
        if let Some(mut state) = self.conns.get_mut(&conn.id) {
            state.device = Some(info.clone());
        }
        self.metrics.inc(&self.metrics.devices_verified_total);
        self.emit(HubEvent::DeviceSeen {
            device: info,
            public_key: p.public_key,
        });
        conn.send(ServerMessage::DeviceVerified { device_id: p.id });
    }

    fn report_event(&self, conn: &mut Connection, kind: String, subject: Option<String>) {
        if !REPORTABLE_EVENTS.contains(&kind.as_str()) {
            return self.fail(conn, ErrorCode::InvalidMessage, "unknown event kind");
        }
        let Some(session) = self.current_session(conn) else {
            return self.fail(conn, ErrorCode::NotInRoom, "not in a room");
        };
        let (actor, subject) = match self.rooms.get(&session.room_id) {
            Some(room) => (
                room.device_of(&session.participant_id).cloned(),
                subject.and_then(|s| room.device_of(&s).cloned()),
            ),
            None => return,
        };
        self.audit(&kind, &session.room_id, actor.as_ref(), subject.as_ref());
    }

    fn audit(
        &self,
        kind: &str,
        code: &str,
        actor: Option<&DeviceInfo>,
        subject: Option<&DeviceInfo>,
    ) {
        if actor.is_none() && subject.is_none() {
            return; // the audit log is per device
        }
        self.emit(HubEvent::Audit(AuditRecord {
            at: SystemTime::now(),
            kind: kind.to_string(),
            room: room_hash(code),
            actor: actor.map(|d| d.id.clone()),
            subject: subject.map(|d| d.id.clone()),
        }));
    }

    fn emit(&self, event: HubEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(event);
        }
    }

    // ----- connection state ------------------------------------------------------

    fn set_attach(&self, conn_id: u64, attach: Attach) {
        if let Some(mut state) = self.conns.get_mut(&conn_id) {
            state.attach = attach;
        }
    }

    /// Current attachment, cleaned up if its room or slot went away.
    fn attach(&self, conn: &Connection) -> Attach {
        let attach = self
            .conns
            .get(&conn.id)
            .map(|c| c.attach.clone())
            .unwrap_or(Attach::None);
        let valid = match &attach {
            Attach::None => true,
            Attach::Waiting {
                room_id,
                request_id,
            } => self
                .rooms
                .get(room_id)
                .is_some_and(|r| r.waiting.contains_key(request_id)),
            Attach::Joined(s) => self.rooms.get(&s.room_id).is_some_and(|r| {
                r.participants
                    .get(&s.participant_id)
                    .is_some_and(|p| p.conn_id == conn.id)
            }),
        };
        if valid {
            attach
        } else {
            self.set_attach(conn.id, Attach::None);
            Attach::None
        }
    }

    /// The connection's session, provided it is still the live connection for
    /// that participant and the room still exists.
    fn current_session(&self, conn: &Connection) -> Option<Session> {
        match self.attach(conn) {
            Attach::Joined(s) => Some(s),
            _ => None,
        }
    }

    fn fail(&self, conn: &Connection, code: ErrorCode, message: &str) {
        self.metrics.error(code);
        conn.send(ServerMessage::error(code, message));
    }
}

fn allowed(limiter: &RateLimiter<IpAddr>, conn: &Connection) -> bool {
    conn.remote_ip.is_none_or(|ip| limiter.check(&ip))
}

/// Unix seconds, for events crossing the wire or the database.
pub fn unix_seconds(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

#[cfg(test)]
mod tests;
