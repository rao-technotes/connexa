//! Transport-agnostic room hub.
//!
//! The hub owns every room in memory and routes protocol messages between
//! participants. It knows nothing about WebSockets: a transport creates a
//! [`Connection`] per client, feeds decoded [`ClientMessage`]s into
//! [`Hub::handle`] and forwards whatever arrives on the connection's outbound
//! channel. That keeps it embeddable in the cloud server as well as in a
//! desktop app acting as a temporary LAN signaling server.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use connexa_core::DEFAULT_MAX_PARTICIPANTS;
use connexa_protocol::{
    ClientMessage, ErrorCode, IceServer, ParticipantId, ParticipantInfo, RoomEndReason,
    ServerMessage,
};
use connexa_security::{
    RateLimiter, constant_time_eq, generate_participant_id, generate_room_code, generate_token,
    normalize_room_code, redact_room_code, sanitize_display_name,
};
use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Channel carrying messages from the hub to one client connection.
pub type Outbound = mpsc::Sender<ServerMessage>;

/// Produces the ICE servers handed to a participant (lets the server mint
/// per-participant, time-limited TURN credentials).
pub type IceProvider = Arc<dyn Fn(&str) -> Vec<IceServer> + Send + Sync>;

/// Messages queued for a participant whose connection dropped, replayed on resume.
const MAX_PENDING_MESSAGES: usize = 512;

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
        }
    }
}

/// One client transport connection (e.g. one WebSocket).
pub struct Connection {
    id: u64,
    tx: Outbound,
    remote_ip: Option<IpAddr>,
    session: Option<Session>,
}

#[derive(Clone)]
struct Session {
    room_id: String,
    participant_id: ParticipantId,
}

impl Connection {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn participant_id(&self) -> Option<&str> {
        self.session.as_ref().map(|s| s.participant_id.as_str())
    }

    /// Send a message to this connection only (e.g. a transport-level error).
    pub fn send(&self, msg: ServerMessage) {
        reply(self, msg);
    }
}

struct Room {
    host_id: ParticipantId,
    participants: HashMap<ParticipantId, Participant>,
    created_at: Instant,
    last_activity: Instant,
}

struct Participant {
    id: ParticipantId,
    display_name: String,
    resume_token: String,
    conn_id: u64,
    tx: Option<Outbound>,
    disconnected_at: Option<Instant>,
    pending: VecDeque<ServerMessage>,
}

impl Participant {
    fn info(&self, host_id: &str) -> ParticipantInfo {
        ParticipantInfo {
            participant_id: self.id.clone(),
            display_name: self.display_name.clone(),
            is_host: self.id == host_id,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HubStats {
    pub rooms: usize,
    pub participants: usize,
}

pub struct Hub {
    config: HubConfig,
    ice: IceProvider,
    rooms: DashMap<String, Room>,
    next_conn_id: AtomicU64,
    join_limiter: RateLimiter<IpAddr>,
    create_limiter: RateLimiter<IpAddr>,
}

impl Hub {
    pub fn new(config: HubConfig, ice: IceProvider) -> Self {
        Self {
            join_limiter: RateLimiter::new(config.join_attempts_per_window, config.rate_window),
            create_limiter: RateLimiter::new(config.creates_per_window, config.rate_window),
            config,
            ice,
            rooms: DashMap::new(),
            next_conn_id: AtomicU64::new(1),
        }
    }

    pub fn config(&self) -> &HubConfig {
        &self.config
    }

    pub fn connect(&self, tx: Outbound, remote_ip: Option<IpAddr>) -> Connection {
        Connection {
            id: self.next_conn_id.fetch_add(1, Ordering::Relaxed),
            tx,
            remote_ip,
            session: None,
        }
    }

    pub fn stats(&self) -> HubStats {
        let mut stats = HubStats {
            rooms: 0,
            participants: 0,
        };
        for room in self.rooms.iter() {
            stats.rooms += 1;
            stats.participants += room.participants.len();
        }
        stats
    }

    pub fn handle(&self, conn: &mut Connection, msg: ClientMessage) {
        match msg {
            ClientMessage::CreateRoom { display_name } => self.create_room(conn, display_name),
            ClientMessage::JoinRoom {
                room_id,
                display_name,
            } => self.join_room(conn, &room_id, display_name),
            ClientMessage::Resume {
                room_id,
                participant_id,
                resume_token,
            } => self.resume(conn, &room_id, &participant_id, &resume_token),
            ClientMessage::LeaveRoom => self.leave(conn),
            ClientMessage::EndRoom => self.end_room(conn),
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
            ClientMessage::Ping => {
                if let Some(session) = &conn.session
                    && let Some(mut room) = self.rooms.get_mut(&session.room_id)
                {
                    room.last_activity = Instant::now();
                }
                reply(conn, ServerMessage::Pong);
            }
        }
    }

    /// The transport closed. The participant keeps its slot for the reconnect
    /// grace period; [`Hub::sweep`] removes it afterwards.
    pub fn disconnect(&self, conn: Connection) {
        let Some(session) = conn.session else { return };
        if let Some(mut room) = self.rooms.get_mut(&session.room_id)
            && let Some(p) = room.participants.get_mut(&session.participant_id)
            && p.conn_id == conn.id
        {
            debug!(participant = %p.id, "connection lost, holding slot for reconnect");
            p.tx = None;
            p.disconnected_at = Some(Instant::now());
        }
    }

    /// Expire rooms and drop participants whose reconnect grace ran out.
    /// Call periodically (every few seconds).
    pub fn sweep(&self, now: Instant) {
        let cfg = &self.config;
        self.rooms.retain(|code, room| {
            let idle = now.saturating_duration_since(room.last_activity) > cfg.room_idle_timeout;
            let too_old = now.saturating_duration_since(room.created_at) > cfg.room_max_lifetime;
            if idle || too_old {
                info!(room = %redact_room_code(code), "room expired");
                room.broadcast(
                    None,
                    &ServerMessage::RoomEnded {
                        reason: RoomEndReason::Expired,
                    },
                );
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
                if id == room.host_id {
                    info!(room = %redact_room_code(code), "host did not reconnect, ending room");
                    room.broadcast(
                        None,
                        &ServerMessage::RoomEnded {
                            reason: RoomEndReason::HostLeft,
                        },
                    );
                    return false;
                }
                room.broadcast(None, &ServerMessage::ParticipantLeft { participant_id: id });
            }
            true
        });
        self.join_limiter.sweep(now);
        self.create_limiter.sweep(now);
    }

    fn create_room(&self, conn: &mut Connection, display_name: Option<String>) {
        if conn.session.is_some() {
            return reply(
                conn,
                err(ErrorCode::AlreadyInRoom, "leave the current room first"),
            );
        }
        if !allowed(&self.create_limiter, conn) {
            return reply(
                conn,
                err(
                    ErrorCode::RateLimited,
                    "too many rooms created, try again later",
                ),
            );
        }
        if self.rooms.len() >= self.config.max_rooms {
            return reply(conn, err(ErrorCode::ServerBusy, "server is at capacity"));
        }

        let now = Instant::now();
        let participant = Participant {
            id: generate_participant_id(),
            display_name: sanitize_display_name(display_name.as_deref(), "Host"),
            resume_token: generate_token(),
            conn_id: conn.id,
            tx: Some(conn.tx.clone()),
            disconnected_at: None,
            pending: VecDeque::new(),
        };

        for _ in 0..16 {
            let code = generate_room_code();
            let Entry::Vacant(slot) = self.rooms.entry(code.clone()) else {
                continue;
            };
            let msg = ServerMessage::RoomCreated {
                room_id: code.clone(),
                participant_id: participant.id.clone(),
                resume_token: participant.resume_token.clone(),
                max_participants: self.config.max_participants,
                ice_servers: (self.ice)(&participant.id),
            };
            conn.session = Some(Session {
                room_id: code.clone(),
                participant_id: participant.id.clone(),
            });
            let host_id = participant.id.clone();
            slot.insert(Room {
                host_id: host_id.clone(),
                participants: HashMap::from([(host_id, participant)]),
                created_at: now,
                last_activity: now,
            });
            info!(room = %redact_room_code(&code), "room created");
            return reply(conn, msg);
        }
        reply(
            conn,
            err(ErrorCode::ServerBusy, "could not allocate a room code"),
        );
    }

    fn join_room(&self, conn: &mut Connection, room_id: &str, display_name: Option<String>) {
        if conn.session.is_some() {
            return reply(
                conn,
                err(ErrorCode::AlreadyInRoom, "leave the current room first"),
            );
        }
        if !allowed(&self.join_limiter, conn) {
            return reply(
                conn,
                err(
                    ErrorCode::RateLimited,
                    "too many join attempts, try again later",
                ),
            );
        }
        let Some(code) = normalize_room_code(room_id) else {
            return reply(
                conn,
                err(ErrorCode::InvalidRoomCode, "room codes are 9 digits"),
            );
        };
        let Some(mut room) = self.rooms.get_mut(&code) else {
            return reply(
                conn,
                err(ErrorCode::RoomNotFound, "no active session with that code"),
            );
        };
        if room.participants.len() >= self.config.max_participants {
            return reply(conn, err(ErrorCode::RoomFull, "this session is full"));
        }

        let guest_no = room.participants.len();
        let participant = Participant {
            id: generate_participant_id(),
            display_name: sanitize_display_name(
                display_name.as_deref(),
                &format!("Guest {guest_no}"),
            ),
            resume_token: generate_token(),
            conn_id: conn.id,
            tx: Some(conn.tx.clone()),
            disconnected_at: None,
            pending: VecDeque::new(),
        };
        let info = participant.info(&room.host_id);
        let msg = ServerMessage::RoomJoined {
            room_id: code.clone(),
            participant_id: participant.id.clone(),
            resume_token: participant.resume_token.clone(),
            host_id: room.host_id.clone(),
            participants: room.infos_except(&participant.id),
            max_participants: self.config.max_participants,
            ice_servers: (self.ice)(&participant.id),
        };

        room.broadcast(
            None,
            &ServerMessage::ParticipantJoined { participant: info },
        );
        room.last_activity = Instant::now();
        conn.session = Some(Session {
            room_id: code.clone(),
            participant_id: participant.id.clone(),
        });
        room.participants
            .insert(participant.id.clone(), participant);
        info!(room = %redact_room_code(&code), size = room.participants.len(), "participant joined");
        reply(conn, msg);
    }

    fn resume(&self, conn: &mut Connection, room_id: &str, participant_id: &str, token: &str) {
        if conn.session.is_some() {
            return reply(
                conn,
                err(ErrorCode::AlreadyInRoom, "already attached to a room"),
            );
        }
        if !allowed(&self.join_limiter, conn) {
            return reply(
                conn,
                err(ErrorCode::RateLimited, "too many attempts, try again later"),
            );
        }
        let failed = || err(ErrorCode::ResumeFailed, "session can no longer be resumed");
        let Some(mut room) = self.rooms.get_mut(room_id) else {
            return reply(conn, failed());
        };
        let host_id = room.host_id.clone();
        let participants = room.infos_except(participant_id);
        let Some(p) = room.participants.get_mut(participant_id) else {
            return reply(conn, failed());
        };
        if !constant_time_eq(&p.resume_token, token) {
            return reply(conn, failed());
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
        conn.session = Some(Session {
            room_id: room_id.to_string(),
            participant_id: participant_id.to_string(),
        });
        debug!(participant = %participant_id, "session resumed");
    }

    fn leave(&self, conn: &mut Connection) {
        let Some(session) = self.current_session(conn) else {
            return reply(conn, err(ErrorCode::NotInRoom, "not in a room"));
        };
        conn.session = None;
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
        if host_left {
            self.close_room(&session.room_id, RoomEndReason::HostLeft);
        }
    }

    fn end_room(&self, conn: &mut Connection) {
        let Some(session) = self.current_session(conn) else {
            return reply(conn, err(ErrorCode::NotInRoom, "not in a room"));
        };
        let is_host = self
            .rooms
            .get(&session.room_id)
            .is_some_and(|r| r.host_id == session.participant_id);
        if !is_host {
            return reply(
                conn,
                err(ErrorCode::NotHost, "only the host can end the session"),
            );
        }
        self.close_room(&session.room_id, RoomEndReason::HostEnded);
    }

    fn close_room(&self, room_id: &str, reason: RoomEndReason) {
        if let Some((code, mut room)) = self.rooms.remove(room_id) {
            info!(room = %redact_room_code(&code), ?reason, "room closed");
            room.broadcast(None, &ServerMessage::RoomEnded { reason });
        }
    }

    fn relay(
        &self,
        conn: &mut Connection,
        target: &str,
        make: impl FnOnce(ParticipantId) -> ServerMessage,
    ) {
        let Some(session) = self.current_session(conn) else {
            return reply(conn, err(ErrorCode::NotInRoom, "not in a room"));
        };
        let Some(mut room) = self.rooms.get_mut(&session.room_id) else {
            return reply(conn, err(ErrorCode::NotInRoom, "room no longer exists"));
        };
        room.last_activity = Instant::now();
        match room.participants.get_mut(target) {
            Some(peer) if peer.id != session.participant_id => {
                peer.deliver(make(session.participant_id));
            }
            _ => reply(
                conn,
                err(ErrorCode::TargetNotFound, "target is not in this room"),
            ),
        }
    }

    /// The connection's session, provided it is still the live connection for
    /// that participant and the room still exists.
    fn current_session(&self, conn: &mut Connection) -> Option<Session> {
        let session = conn.session.clone()?;
        let live = self.rooms.get(&session.room_id).is_some_and(|room| {
            room.participants
                .get(&session.participant_id)
                .is_some_and(|p| p.conn_id == conn.id)
        });
        if !live {
            conn.session = None;
        }
        live.then_some(session)
    }
}

fn allowed(limiter: &RateLimiter<IpAddr>, conn: &Connection) -> bool {
    conn.remote_ip.is_none_or(|ip| limiter.check(&ip))
}

fn err(code: ErrorCode, message: &str) -> ServerMessage {
    ServerMessage::error(code, message)
}

fn reply(conn: &Connection, msg: ServerMessage) {
    if let Err(e) = conn.tx.try_send(msg) {
        warn!(conn = conn.id, "dropping reply: {e}");
    }
}

#[cfg(test)]
mod tests;
