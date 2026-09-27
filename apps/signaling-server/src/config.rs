//! Server configuration, read from `CONNEXA_*` environment variables.

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use connexa_protocol::IceServer;
use connexa_security::turn_rest_credentials;
use connexa_signaling::{HubConfig, IceProvider};

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub hub: HubConfig,
    /// Directory holding the built web client (`clients/web/dist`).
    pub web_root: PathBuf,
    /// Allowed `Origin` headers for WebSocket upgrades. Empty = allow all (development).
    pub allowed_origins: Vec<String>,
    /// Use the first `X-Forwarded-For` address as client IP (only behind a trusted proxy).
    pub trust_proxy: bool,
    /// Close a WebSocket that sends nothing (not even a ping) for this long.
    pub socket_idle_timeout: Duration,
    pub ice: IceConfig,
}

#[derive(Debug, Clone, Default)]
pub struct IceConfig {
    pub stun_urls: Vec<String>,
    pub turn_urls: Vec<String>,
    /// Shared secret with coturn (`static-auth-secret`); preferred over static credentials.
    pub turn_secret: Option<String>,
    pub turn_username: Option<String>,
    pub turn_credential: Option<String>,
    pub turn_ttl: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8080".parse().unwrap(),
            hub: HubConfig::default(),
            web_root: PathBuf::from("clients/web/dist"),
            allowed_origins: Vec::new(),
            trust_proxy: false,
            socket_idle_timeout: Duration::from_secs(60),
            ice: IceConfig {
                stun_urls: vec!["stun:stun.l.google.com:19302".into()],
                turn_ttl: Duration::from_secs(6 * 60 * 60),
                ..Default::default()
            },
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let mut c = Config::default();
        if let Some(v) = var("CONNEXA_BIND") {
            c.bind = v.parse().context("CONNEXA_BIND")?;
        }
        parse_into(&mut c.hub.max_participants, "CONNEXA_MAX_PARTICIPANTS")?;
        parse_into(&mut c.hub.max_rooms, "CONNEXA_MAX_ROOMS")?;
        parse_secs(
            &mut c.hub.room_idle_timeout,
            "CONNEXA_ROOM_IDLE_TIMEOUT_SECS",
        )?;
        parse_secs(
            &mut c.hub.room_max_lifetime,
            "CONNEXA_ROOM_MAX_LIFETIME_SECS",
        )?;
        parse_secs(&mut c.hub.reconnect_grace, "CONNEXA_RECONNECT_GRACE_SECS")?;
        parse_into(
            &mut c.hub.join_attempts_per_window,
            "CONNEXA_JOIN_ATTEMPTS_PER_MINUTE",
        )?;
        parse_into(&mut c.hub.creates_per_window, "CONNEXA_CREATES_PER_MINUTE")?;
        parse_secs(
            &mut c.socket_idle_timeout,
            "CONNEXA_SOCKET_IDLE_TIMEOUT_SECS",
        )?;
        if let Some(v) = var("CONNEXA_WEB_ROOT") {
            c.web_root = v.into();
        }
        if let Some(v) = var("CONNEXA_ALLOWED_ORIGINS") {
            c.allowed_origins = list(&v);
        }
        if let Some(v) = var("CONNEXA_TRUST_PROXY") {
            c.trust_proxy = matches!(v.as_str(), "1" | "true" | "yes");
        }
        if let Some(v) = var("CONNEXA_STUN_URLS") {
            c.ice.stun_urls = list(&v);
        }
        if let Some(v) = var("CONNEXA_TURN_URLS") {
            c.ice.turn_urls = list(&v);
        }
        c.ice.turn_secret = var("CONNEXA_TURN_SECRET");
        c.ice.turn_username = var("CONNEXA_TURN_USERNAME");
        c.ice.turn_credential = var("CONNEXA_TURN_CREDENTIAL");
        parse_secs(&mut c.ice.turn_ttl, "CONNEXA_TURN_TTL_SECS")?;

        anyhow::ensure!(
            c.hub.max_participants >= 2,
            "CONNEXA_MAX_PARTICIPANTS must be at least 2"
        );
        Ok(c)
    }
}

impl IceConfig {
    pub fn provider(&self) -> IceProvider {
        let ice = self.clone();
        Arc::new(move |participant_id: &str| {
            let mut servers = Vec::new();
            if !ice.stun_urls.is_empty() {
                servers.push(IceServer {
                    urls: ice.stun_urls.clone(),
                    username: None,
                    credential: None,
                });
            }
            if !ice.turn_urls.is_empty() {
                let creds = match (&ice.turn_secret, &ice.turn_username, &ice.turn_credential) {
                    (Some(secret), _, _) => {
                        Some(turn_rest_credentials(secret, participant_id, ice.turn_ttl))
                    }
                    (None, Some(u), Some(p)) => Some((u.clone(), p.clone())),
                    _ => None,
                };
                if let Some((username, credential)) = creds {
                    servers.push(IceServer {
                        urls: ice.turn_urls.clone(),
                        username: Some(username),
                        credential: Some(credential),
                    });
                }
            }
            servers
        })
    }
}

fn var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn list(v: &str) -> Vec<String> {
    v.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn parse_into<T: FromStr>(target: &mut T, name: &str) -> Result<()>
where
    T::Err: std::error::Error + Send + Sync + 'static,
{
    if let Some(v) = var(name) {
        *target = v.parse().with_context(|| format!("invalid {name}"))?;
    }
    Ok(())
}

fn parse_secs(target: &mut Duration, name: &str) -> Result<()> {
    let mut secs = target.as_secs();
    parse_into(&mut secs, name)?;
    *target = Duration::from_secs(secs);
    Ok(())
}
