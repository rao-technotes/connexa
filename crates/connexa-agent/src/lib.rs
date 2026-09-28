//! Native agent for remote control.
//!
//! Everything privileged goes through [`ControlGate`]: a remote participant
//! can only inject input after the local user explicitly granted the matching
//! [`Permission`], and grants can be revoked at any moment. Revoking releases
//! any key or mouse button the remote side was still holding.

mod keymap;
#[cfg(windows)]
mod windows;

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

pub use keymap::scancode;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    Mouse,
    Keyboard,
    Clipboard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

/// Input from the remote viewer. Pointer coordinates are normalized to the
/// shared screen: `0.0..=1.0` from its left/top edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "e", rename_all = "snake_case")]
pub enum InputEvent {
    Move {
        x: f64,
        y: f64,
    },
    Down {
        x: f64,
        y: f64,
        button: MouseButton,
    },
    Up {
        x: f64,
        y: f64,
        button: MouseButton,
    },
    /// Browser `WheelEvent` deltas in pixels (≈100 per notch).
    Wheel {
        dx: f64,
        dy: f64,
    },
    /// `KeyboardEvent.code`, e.g. `"KeyA"`, `"ShiftLeft"`, `"ArrowUp"`.
    Key {
        code: String,
        down: bool,
    },
}

impl InputEvent {
    pub fn required_permission(&self) -> Permission {
        match self {
            InputEvent::Key { .. } => Permission::Keyboard,
            _ => Permission::Mouse,
        }
    }
}

/// A monitor in physical pixels on the virtual desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    /// Physical pixel for a normalized point, clamped inside the rectangle.
    pub fn point(&self, nx: f64, ny: f64) -> (i32, i32) {
        let clamp = |v: f64| {
            if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                0.0
            }
        };
        let px = self.x + (clamp(nx) * (self.width.saturating_sub(1)) as f64).round() as i32;
        let py = self.y + (clamp(ny) * (self.height.saturating_sub(1)) as f64).round() as i32;
        (px, py)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AgentError {
    #[error("{0:?} control was not granted to this participant")]
    NotGranted(Permission),
    #[error("unknown key code {0:?}")]
    UnknownKey(String),
    #[error("input injection is not supported on this platform")]
    Unsupported,
    #[error(
        "the system blocked input injection (e.g. an elevated window or the secure desktop is focused)"
    )]
    Blocked,
    #[error("clipboard error: {0}")]
    Clipboard(String),
}

/// Something that can deliver input to the OS. Swappable for tests.
pub trait InputSink: Send {
    fn send(&mut self, event: &InputEvent, screen: Rect) -> Result<(), AgentError>;
}

/// Injects through the operating system (SendInput on Windows).
#[derive(Default)]
pub struct SystemInput;

impl InputSink for SystemInput {
    #[cfg(windows)]
    fn send(&mut self, event: &InputEvent, screen: Rect) -> Result<(), AgentError> {
        windows::inject(event, screen)
    }

    #[cfg(not(windows))]
    fn send(&mut self, _event: &InputEvent, _screen: Rect) -> Result<(), AgentError> {
        Err(AgentError::Unsupported)
    }
}

pub fn input_supported() -> bool {
    cfg!(windows)
}

#[derive(Debug, Clone)]
struct Grant {
    permissions: HashSet<Permission>,
    screen: Rect,
    held_keys: HashSet<String>,
    held_buttons: HashSet<MouseButton>,
}

/// Tracks who may control this machine, and how.
pub struct ControlGate<S: InputSink = SystemInput> {
    sink: S,
    grants: HashMap<String, Grant>,
}

impl Default for ControlGate<SystemInput> {
    fn default() -> Self {
        Self::new(SystemInput)
    }
}

impl<S: InputSink> ControlGate<S> {
    pub fn new(sink: S) -> Self {
        Self {
            sink,
            grants: HashMap::new(),
        }
    }

    /// Grant `peer` the given permissions over `screen`. Replaces any earlier grant.
    pub fn grant(&mut self, peer: &str, permissions: &[Permission], screen: Rect) {
        self.revoke(peer);
        self.grants.insert(
            peer.to_string(),
            Grant {
                permissions: permissions.iter().copied().collect(),
                screen,
                held_keys: HashSet::new(),
                held_buttons: HashSet::new(),
            },
        );
    }

    pub fn permissions(&self, peer: &str) -> Vec<Permission> {
        self.grants
            .get(peer)
            .map(|g| g.permissions.iter().copied().collect())
            .unwrap_or_default()
    }

    pub fn has(&self, peer: &str, permission: Permission) -> bool {
        self.grants
            .get(peer)
            .is_some_and(|g| g.permissions.contains(&permission))
    }

    /// Remove `peer`'s grant and release anything it was holding down.
    pub fn revoke(&mut self, peer: &str) {
        let Some(grant) = self.grants.remove(peer) else {
            return;
        };
        for code in grant.held_keys {
            let _ = self
                .sink
                .send(&InputEvent::Key { code, down: false }, grant.screen);
        }
        for button in grant.held_buttons {
            let _ = self.sink.send(
                &InputEvent::Up {
                    x: 0.5,
                    y: 0.5,
                    button,
                },
                grant.screen,
            );
        }
    }

    pub fn revoke_all(&mut self) {
        let peers: Vec<String> = self.grants.keys().cloned().collect();
        for peer in peers {
            self.revoke(&peer);
        }
    }

    pub fn active_peers(&self) -> Vec<String> {
        self.grants.keys().cloned().collect()
    }

    /// Inject `event` on behalf of `peer` if, and only if, it holds the permission.
    pub fn inject(&mut self, peer: &str, event: &InputEvent) -> Result<(), AgentError> {
        let needed = event.required_permission();
        let grant = self
            .grants
            .get_mut(peer)
            .filter(|g| g.permissions.contains(&needed))
            .ok_or(AgentError::NotGranted(needed))?;
        if let InputEvent::Key { code, .. } = event
            && scancode(code).is_none()
        {
            return Err(AgentError::UnknownKey(code.clone()));
        }
        self.sink.send(event, grant.screen)?;
        match event {
            InputEvent::Key { code, down: true } => {
                grant.held_keys.insert(code.clone());
            }
            InputEvent::Key { code, down: false } => {
                grant.held_keys.remove(code);
            }
            InputEvent::Down { button, .. } => {
                grant.held_buttons.insert(*button);
            }
            InputEvent::Up { button, .. } => {
                grant.held_buttons.remove(button);
            }
            _ => {}
        }
        Ok(())
    }
}

/// OS clipboard (text only).
pub mod clipboard {
    use super::AgentError;

    pub fn read_text() -> Result<String, AgentError> {
        arboard::Clipboard::new()
            .and_then(|mut c| c.get_text())
            .map_err(|e| AgentError::Clipboard(e.to_string()))
    }

    pub fn write_text(text: &str) -> Result<(), AgentError> {
        arboard::Clipboard::new()
            .and_then(|mut c| c.set_text(text.to_string()))
            .map_err(|e| AgentError::Clipboard(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<InputEvent>>>);

    impl InputSink for Recorder {
        fn send(&mut self, event: &InputEvent, _screen: Rect) -> Result<(), AgentError> {
            self.0.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    const SCREEN: Rect = Rect {
        x: -1920,
        y: 0,
        width: 1920,
        height: 1080,
    };

    fn key(code: &str, down: bool) -> InputEvent {
        InputEvent::Key {
            code: code.into(),
            down,
        }
    }

    #[test]
    fn nothing_is_injected_without_a_grant() {
        let rec = Recorder::default();
        let mut gate = ControlGate::new(rec.clone());
        assert_eq!(
            gate.inject("bob", &InputEvent::Move { x: 0.5, y: 0.5 }),
            Err(AgentError::NotGranted(Permission::Mouse))
        );
        assert!(rec.0.lock().unwrap().is_empty());
    }

    #[test]
    fn permissions_are_granular() {
        let rec = Recorder::default();
        let mut gate = ControlGate::new(rec.clone());
        gate.grant("bob", &[Permission::Mouse], SCREEN);
        assert!(
            gate.inject("bob", &InputEvent::Move { x: 0.1, y: 0.2 })
                .is_ok()
        );
        assert_eq!(
            gate.inject("bob", &key("KeyA", true)),
            Err(AgentError::NotGranted(Permission::Keyboard))
        );
        assert_eq!(
            gate.inject("carol", &InputEvent::Move { x: 0.1, y: 0.2 }),
            Err(AgentError::NotGranted(Permission::Mouse))
        );
        assert_eq!(rec.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn revoke_releases_held_keys_and_buttons() {
        let rec = Recorder::default();
        let mut gate = ControlGate::new(rec.clone());
        gate.grant("bob", &[Permission::Mouse, Permission::Keyboard], SCREEN);
        gate.inject("bob", &key("ShiftLeft", true)).unwrap();
        gate.inject(
            "bob",
            &InputEvent::Down {
                x: 0.5,
                y: 0.5,
                button: MouseButton::Left,
            },
        )
        .unwrap();
        rec.0.lock().unwrap().clear();

        gate.revoke("bob");
        let released = rec.0.lock().unwrap().clone();
        assert!(released.contains(&key("ShiftLeft", false)));
        assert!(released.iter().any(|e| matches!(
            e,
            InputEvent::Up {
                button: MouseButton::Left,
                ..
            }
        )));
        assert!(!gate.has("bob", Permission::Mouse));
        assert!(gate.inject("bob", &key("KeyA", true)).is_err());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let mut gate = ControlGate::new(Recorder::default());
        gate.grant("bob", &[Permission::Keyboard], SCREEN);
        assert_eq!(
            gate.inject("bob", &key("LaunchRocket", true)),
            Err(AgentError::UnknownKey("LaunchRocket".into()))
        );
    }

    #[test]
    fn normalized_points_map_into_the_monitor() {
        assert_eq!(SCREEN.point(0.0, 0.0), (-1920, 0));
        assert_eq!(SCREEN.point(1.0, 1.0), (-1, 1079));
        assert_eq!(SCREEN.point(0.5, 0.5), (-960, 540));
        assert_eq!(SCREEN.point(-3.0, f64::NAN), (-1920, 0));
        assert_eq!(SCREEN.point(7.0, 2.0), (-1, 1079));
    }

    #[test]
    fn events_use_compact_json() {
        let e: InputEvent =
            serde_json::from_str(r#"{"e":"down","x":0.25,"y":0.75,"button":"right"}"#).unwrap();
        assert_eq!(
            e,
            InputEvent::Down {
                x: 0.25,
                y: 0.75,
                button: MouseButton::Right
            }
        );
        let k: InputEvent =
            serde_json::from_str(r#"{"e":"key","code":"KeyA","down":true}"#).unwrap();
        assert_eq!(k.required_permission(), Permission::Keyboard);
    }
}
