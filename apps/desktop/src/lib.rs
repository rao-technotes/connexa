//! Connexa desktop (Windows): the shared web client in a WebView2 window, plus
//! native capabilities exposed as Tauri commands:
//!
//! - remote control (mouse / keyboard) through [`connexa_agent::ControlGate`],
//!   granted only after a native Windows confirmation the web layer can't bypass
//! - clipboard access
//! - saving received files to the Downloads folder
//! - LAN mode: an embedded signaling server advertised over mDNS

mod lan;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use connexa_agent::{ControlGate, InputEvent, Permission, Rect, clipboard, input_supported};
use serde::{Deserialize, Serialize};
use tauri::{Manager, State, WindowEvent};

#[derive(Default)]
struct Agent {
    gate: Mutex<ControlGate>,
}

#[derive(Serialize)]
struct AgentInfo {
    platform: &'static str,
    input: bool,
    version: &'static str,
}

#[derive(Serialize, Deserialize, Clone)]
struct MonitorInfo {
    name: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    primary: bool,
}

#[tauri::command]
fn agent_info() -> AgentInfo {
    AgentInfo {
        platform: std::env::consts::OS,
        input: input_supported(),
        version: env!("CARGO_PKG_VERSION"),
    }
}

#[tauri::command]
fn monitors(window: tauri::WebviewWindow) -> Result<Vec<MonitorInfo>, String> {
    let primary = window
        .primary_monitor()
        .ok()
        .flatten()
        .map(|m| (m.position().x, m.position().y));
    let list = window.available_monitors().map_err(|e| e.to_string())?;
    Ok(list
        .into_iter()
        .enumerate()
        .map(|(i, m)| MonitorInfo {
            name: m
                .name()
                .cloned()
                .unwrap_or_else(|| format!("Display {}", i + 1)),
            x: m.position().x,
            y: m.position().y,
            width: m.size().width,
            height: m.size().height,
            primary: Some((m.position().x, m.position().y)) == primary,
        })
        .collect())
}

#[tauri::command]
async fn control_grant(
    agent: State<'_, Agent>,
    peer_id: String,
    peer_name: String,
    permissions: Vec<Permission>,
    monitor: MonitorInfo,
) -> Result<bool, String> {
    if permissions.is_empty() || !input_supported() {
        return Ok(false);
    }
    let names: Vec<&str> = permissions
        .iter()
        .map(|p| match p {
            Permission::Mouse => "mouse",
            Permission::Keyboard => "keyboard",
            Permission::Clipboard => "clipboard",
        })
        .collect();
    let peer_name: String = peer_name
        .chars()
        .filter(|c| !c.is_control())
        .take(32)
        .collect();
    let text = format!(
        "Allow \"{peer_name}\" to control this computer?\n\n\
         Access: {}\n\
         Screen: {} ({}×{})\n\n\
         Only allow people you trust. You can stop control at any time with the \
         \"Stop control\" button in Connexa, or by closing Connexa.",
        names.join(", "),
        monitor.name,
        monitor.width,
        monitor.height,
    );
    let approved = tauri::async_runtime::spawn_blocking(move || {
        confirm("Connexa: remote control request", &text)
    })
    .await
    .map_err(|e| e.to_string())?;
    if approved {
        let screen = Rect {
            x: monitor.x,
            y: monitor.y,
            width: monitor.width,
            height: monitor.height,
        };
        agent
            .gate
            .lock()
            .unwrap()
            .grant(&peer_id, &permissions, screen);
    }
    Ok(approved)
}

#[tauri::command]
async fn control_revoke(agent: State<'_, Agent>, peer_id: String) -> Result<(), String> {
    agent.gate.lock().unwrap().revoke(&peer_id);
    Ok(())
}

#[tauri::command]
async fn control_input(
    agent: State<'_, Agent>,
    peer_id: String,
    input: InputEvent,
) -> Result<(), String> {
    agent
        .gate
        .lock()
        .unwrap()
        .inject(&peer_id, &input)
        .map_err(|e| e.to_string())
}

/// Remote clipboard writes require the clipboard permission.
#[tauri::command]
async fn clipboard_write(
    agent: State<'_, Agent>,
    peer_id: String,
    text: String,
) -> Result<(), String> {
    if !agent
        .gate
        .lock()
        .unwrap()
        .has(&peer_id, Permission::Clipboard)
    {
        return Err("clipboard access was not granted".into());
    }
    clipboard::write_text(&text).map_err(|e| e.to_string())
}

/// Local user action ("share my clipboard"): no grant needed.
#[tauri::command]
async fn clipboard_read() -> Result<String, String> {
    clipboard::read_text().map_err(|e| e.to_string())
}

/// Save a received file (raw bytes body, `x-file-name` header) into Downloads.
#[tauri::command]
async fn save_file(
    app: tauri::AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<String, String> {
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("expected a binary body".into());
    };
    let name = request
        .headers()
        .get("x-file-name")
        .and_then(|v| v.to_str().ok())
        .map(percent_decode)
        .unwrap_or_else(|| "file".into());
    let dir = app.path().download_dir().map_err(|e| e.to_string())?;
    let path = unique_path(&dir, &sanitize_file_name(&name));
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}

#[tauri::command]
fn qr_svg(text: String) -> Result<String, String> {
    let code = qrcode::QrCode::new(text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(240, 240)
        .quiet_zone(true)
        .build())
}

#[cfg(windows)]
fn confirm(title: &str, text: &str) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MessageBoxW,
    };
    let wide = |s: &str| {
        s.encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };
    let (text, title) = (wide(text), wide(title));
    // SAFETY: both strings are NUL-terminated UTF-16 buffers that outlive the call.
    let answer = unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_YESNO | MB_ICONWARNING | MB_TOPMOST | MB_SETFOREGROUND | MB_DEFBUTTON2,
        )
    };
    answer == IDYES
}

#[cfg(not(windows))]
fn confirm(_title: &str, _text: &str) -> bool {
    false
}

fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_control() || r#"<>:"/\|?*"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    // Reserved device names (CON, NUL, COM1…) would not create a file.
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4);
    if cleaned.is_empty() || reserved {
        format!("file_{cleaned}")
    } else {
        cleaned.chars().take(120).collect()
    }
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("an unused name exists")
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn run() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    tauri::Builder::default()
        .manage(Agent::default())
        .manage(lan::Lan::default())
        .invoke_handler(tauri::generate_handler![
            agent_info,
            monitors,
            control_grant,
            control_revoke,
            control_input,
            clipboard_write,
            clipboard_read,
            save_file,
            qr_svg,
            lan::lan_start,
            lan::lan_stop,
            lan::lan_discover,
        ])
        .on_window_event(|window, event| {
            if let WindowEvent::Destroyed = event {
                // Never leave anyone in control once the window is gone.
                window.state::<Agent>().gate.lock().unwrap().revoke_all();
                window.state::<lan::Lan>().stop_now();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Connexa");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_sanitized() {
        assert_eq!(sanitize_file_name("report.pdf"), "report.pdf");
        assert_eq!(sanitize_file_name("../../evil.exe"), "_.._evil.exe");
        assert_eq!(sanitize_file_name("a<b>c:d.txt"), "a_b_c_d.txt");
        assert_eq!(sanitize_file_name("CON.txt"), "file_CON.txt");
        assert_eq!(sanitize_file_name("   "), "file_");
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("na%C3%AFve%20file.txt"), "naïve file.txt");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn unique_paths_do_not_overwrite() {
        let dir = std::env::temp_dir().join(format!("connexa-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (1).txt"));
        assert_eq!(unique_path(&dir, "b.txt"), dir.join("b.txt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
