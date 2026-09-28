//! LAN mode: the desktop app runs a temporary signaling server on the local
//! network and advertises it with mDNS (`_connexa._tcp.local.`). The session
//! code is still required to join; discovery only finds the host.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::Serialize;
use signaling_server::config::IceConfig;
use signaling_server::{AppState, Config, serve_until};
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Manager, State};
use tokio::sync::oneshot;
use tracing::{info, warn};

const SERVICE: &str = "_connexa._tcp.local.";
const PREFERRED_PORT: u16 = 47800;

#[derive(Serialize, Clone)]
pub struct LanInfo {
    port: u16,
    /// LAN addresses other devices can use, best first.
    addresses: Vec<String>,
    host: String,
}

#[derive(Serialize)]
pub struct LanPeer {
    name: String,
    host: String,
    port: u16,
}

struct Running {
    info: LanInfo,
    stop: oneshot::Sender<()>,
    mdns: Option<(ServiceDaemon, String)>,
}

#[derive(Default)]
pub struct Lan {
    running: Mutex<Option<Running>>,
    /// Our own advertised instance, hidden from discovery results.
    own_fullname: Mutex<Option<String>>,
}

impl Lan {
    pub fn stop_now(&self) {
        if let Some(r) = self.running.lock().unwrap().take() {
            let _ = r.stop.send(());
            if let Some((daemon, fullname)) = r.mdns {
                let _ = daemon.unregister(&fullname);
                let _ = daemon.shutdown();
            }
            *self.own_fullname.lock().unwrap() = None;
            info!("LAN server stopped");
        }
    }
}

#[tauri::command]
pub async fn lan_start(
    app: AppHandle,
    lan: State<'_, Lan>,
    name: String,
) -> Result<LanInfo, String> {
    if let Some(r) = lan.running.lock().unwrap().as_ref() {
        return Ok(r.info.clone());
    }

    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", PREFERRED_PORT)).await {
        Ok(l) => l,
        Err(_) => tokio::net::TcpListener::bind(("0.0.0.0", 0))
            .await
            .map_err(|e| format!("could not open a LAN port: {e}"))?,
    };
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();

    let web_root = app
        .path()
        .resolve("web", BaseDirectory::Resource)
        .map_err(|e| e.to_string())?;
    let config = Config {
        bind: SocketAddr::from(([0, 0, 0, 0], port)),
        web_root,
        // Same network: host candidates connect directly, no STUN/TURN needed.
        ice: IceConfig::default(),
        ..Config::default()
    };
    let (stop, stopped) = oneshot::channel::<()>();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = serve_until(listener, AppState::new(config), async {
            let _ = stopped.await;
        })
        .await
        {
            warn!("LAN server failed: {e}");
        }
    });

    let addresses: Vec<String> = lan_ip().map(|ip| ip.to_string()).into_iter().collect();
    let host = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "connexa-host".into());
    let display = if name.trim().is_empty() {
        host.clone()
    } else {
        name.trim().chars().take(32).collect()
    };
    let mdns = lan_ip().and_then(|ip| {
        advertise(&host, &display, ip, port)
            .map_err(|e| warn!("mDNS: {e}"))
            .ok()
    });
    if let Some((_, fullname)) = &mdns {
        *lan.own_fullname.lock().unwrap() = Some(fullname.clone());
    }

    let info = LanInfo {
        port,
        addresses,
        host,
    };
    info!(port, "LAN server started");
    *lan.running.lock().unwrap() = Some(Running {
        info: info.clone(),
        stop,
        mdns,
    });
    Ok(info)
}

#[tauri::command]
pub async fn lan_stop(lan: State<'_, Lan>) -> Result<(), String> {
    lan.stop_now();
    Ok(())
}

#[tauri::command]
pub async fn lan_discover(
    lan: State<'_, Lan>,
    timeout_ms: Option<u64>,
) -> Result<Vec<LanPeer>, String> {
    let own = lan.own_fullname.lock().unwrap().clone();
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(2500).clamp(500, 10_000));
    tauri::async_runtime::spawn_blocking(move || discover(timeout, own.as_deref()))
        .await
        .map_err(|e| e.to_string())?
}

fn discover(timeout: Duration, own: Option<&str>) -> Result<Vec<LanPeer>, String> {
    let daemon = ServiceDaemon::new().map_err(|e| e.to_string())?;
    let events = daemon.browse(SERVICE).map_err(|e| e.to_string())?;
    let deadline = Instant::now() + timeout;
    let mut found: Vec<LanPeer> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if Some(info.get_fullname()) == own {
                    continue;
                }
                let Some(ip) = info.get_addresses().iter().find(|ip| ip.is_ipv4()).copied() else {
                    continue;
                };
                let host = ip.to_string();
                let port = info.get_port();
                if found.iter().any(|p| p.host == host && p.port == port) {
                    continue;
                }
                let name = info
                    .get_property_val_str("name")
                    .map(|s| s.chars().filter(|c| !c.is_control()).take(32).collect())
                    .unwrap_or_else(|| host.clone());
                found.push(LanPeer { name, host, port });
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.stop_browse(SERVICE);
    let _ = daemon.shutdown();
    Ok(found)
}

fn advertise(
    host: &str,
    name: &str,
    ip: IpAddr,
    port: u16,
) -> Result<(ServiceDaemon, String), String> {
    let daemon = ServiceDaemon::new().map_err(|e| e.to_string())?;
    let suffix: String = format!("{:x}", std::process::id());
    let instance = format!("Connexa {host} {suffix}");
    let props = [("v", "1"), ("name", name)];
    let info = ServiceInfo::new(
        SERVICE,
        &instance,
        &format!("{host}.local."),
        ip,
        port,
        &props[..],
    )
    .map_err(|e| e.to_string())?;
    let fullname = info.get_fullname().to_string();
    daemon.register(info).map_err(|e| e.to_string())?;
    Ok((daemon, fullname))
}

/// The address of the interface used for outbound traffic. `connect` on a UDP
/// socket only selects a route; nothing is sent.
fn lan_ip() -> Option<IpAddr> {
    ["8.8.8.8:80", "10.255.255.255:1", "192.168.255.255:1"]
        .iter()
        .find_map(|target| {
            let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
            socket.connect(target).ok()?;
            let ip = socket.local_addr().ok()?.ip();
            (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
        })
}
