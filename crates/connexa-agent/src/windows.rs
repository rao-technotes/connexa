//! Windows input injection via `SendInput`.
//!
//! Limits imposed by Windows (by design): input cannot reach windows of a
//! process running at a higher integrity level (UIPI) nor the secure desktop
//! (UAC prompts, Ctrl+Alt+Del, lock screen).

use std::mem::size_of;

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::{AgentError, InputEvent, MouseButton, Rect, scancode};

/// Browser wheel pixels per Windows wheel notch (`WHEEL_DELTA` = 120).
const PIXELS_PER_NOTCH: f64 = 100.0;
const WHEEL_DELTA: f64 = 120.0;

pub fn inject(event: &InputEvent, screen: Rect) -> Result<(), AgentError> {
    let inputs: Vec<INPUT> = match event {
        InputEvent::Move { x, y } => vec![move_to(screen, *x, *y)],
        InputEvent::Down { x, y, button } => {
            vec![
                move_to(screen, *x, *y),
                mouse(0, 0, 0, button_flag(*button, true)),
            ]
        }
        InputEvent::Up { x, y, button } => {
            vec![
                move_to(screen, *x, *y),
                mouse(0, 0, 0, button_flag(*button, false)),
            ]
        }
        InputEvent::Wheel { dx, dy } => {
            let mut v = Vec::new();
            let notches =
                |d: f64| ((d / PIXELS_PER_NOTCH) * WHEEL_DELTA).clamp(-1200.0, 1200.0) as i32;
            if *dy != 0.0 {
                // Browser: positive deltaY scrolls down; Windows: positive is up.
                v.push(mouse(0, 0, -notches(*dy), MOUSEEVENTF_WHEEL));
            }
            if *dx != 0.0 {
                v.push(mouse(0, 0, notches(*dx), MOUSEEVENTF_HWHEEL));
            }
            v
        }
        InputEvent::Key { code, down } => {
            let (scan, extended) =
                scancode(code).ok_or_else(|| AgentError::UnknownKey(code.clone()))?;
            let mut flags = KEYEVENTF_SCANCODE;
            if extended {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            vec![INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: 0,
                        wScan: scan,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            }]
        }
    };
    if inputs.is_empty() {
        return Ok(());
    }
    // SAFETY: `inputs` is a valid, initialized slice of INPUT structures.
    let sent = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        )
    };
    if sent as usize == inputs.len() {
        Ok(())
    } else {
        Err(AgentError::Blocked)
    }
}

fn move_to(screen: Rect, nx: f64, ny: f64) -> INPUT {
    let (px, py) = screen.point(nx, ny);
    // SAFETY: GetSystemMetrics has no preconditions.
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
        )
    };
    // Absolute coordinates are 0..=65535 across the whole virtual desktop.
    let ax = (((px - vx) as f64) * 65535.0 / (vw - 1) as f64).round() as i32;
    let ay = (((py - vy) as f64) * 65535.0 / (vh - 1) as f64).round() as i32;
    mouse(
        ax,
        ay,
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )
}

fn button_flag(button: MouseButton, down: bool) -> u32 {
    match (button, down) {
        (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
        (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
        (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as _,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
