//! Exact-target macOS window primitives (private-fork port of the Swift
//! `SetWindowBoundsTool` / `FocusWindowTool` / `CloseWindowTool` /
//! `WindowLifecycleTool` reference implementations):
//!
//! * `set_window_bounds` — AX size-then-position write with observed-window
//!   verification against CGWindowList.
//! * `focus_window` — AXRaise + app activation, verified by the observed
//!   frontmost window.
//! * `close_window` — no-raise focus, normal AXCloseButton press, verified by
//!   the window disappearing from the on-screen list.
//! * `minimize_window` / `restore_window` / `maximize_window` — AXMinimized
//!   writes and the standard zoom button, with observed-window verification.
//!
//! All tools require an exact `pid` + `window_id` match; none of them ever
//! picks another window when the requested one is missing. All failures are
//! loud errors — there is no fallback path.

use async_trait::async_trait;
use cua_driver_core::{protocol::ToolResult, tool::{Tool, ToolDef}};
use serde_json::Value;
use std::time::{Duration, Instant};

use crate::ax::bindings::{
    ax_get_window_id, copy_ax_windows, copy_children, copy_element_attr, copy_number_attr,
    copy_string_attr, perform_action, set_bool_attr, set_point_attr, set_size_attr,
    kAXErrorSuccess, AXUIElementCreateApplication, AXUIElementRef,
};
use crate::windows::{WindowBounds, WindowInfo};
use core_foundation::base::{CFRelease, CFTypeRef};

const OBSERVATION_DEADLINE: Duration = Duration::from_millis(1_500);
const OBSERVATION_POLL: Duration = Duration::from_millis(50);
const BOUNDS_TOLERANCE: f64 = 2.0;

// ── Shared helpers ───────────────────────────────────────────────────────────

/// Find the AX window element for an exact `pid` + CGWindowID pair.
/// Returns a retained element the caller must `CFRelease`.
///
/// Unions `AXWindows` with the app element's `AXChildren` (the same union
/// the tree walker uses): `AXWindows` is the only way to see background
/// windows, but it is also observed to intermittently return empty for
/// apps that do expose their windows via `AXChildren`.
fn find_ax_window(pid: i32, window_id: u32) -> Result<AXUIElementRef, String> {
    unsafe {
        let app = AXUIElementCreateApplication(pid);
        if app.is_null() {
            return Err(format!("No AX application element is available for pid {pid}."));
        }
        let mut windows = copy_ax_windows(app);
        for child in copy_children(app) {
            if windows.iter().any(|&w| w == child) {
                CFRelease(child as CFTypeRef);
                continue;
            }
            let role = copy_string_attr(child, "AXRole").unwrap_or_default();
            if role == "AXWindow" {
                windows.push(child);
            } else {
                CFRelease(child as CFTypeRef);
            }
        }
        CFRelease(app as CFTypeRef);
        if windows.is_empty() {
            return Err(format!("No AX windows are available for pid {pid}."));
        }
        let mut found: Option<AXUIElementRef> = None;
        for window in windows {
            if found.is_none() && ax_window_number(window) == Some(window_id) {
                found = Some(window);
            } else {
                CFRelease(window as CFTypeRef);
            }
        }
        found.ok_or_else(|| {
            format!("No AX window with window_id {window_id} is available for pid {pid}.")
        })
    }
}

/// CGWindowID for an AX window element: private SPI first, `AXWindowNumber`
/// attribute fallback (mirrors the Swift reference's `axWindowNumber`).
unsafe fn ax_window_number(window: AXUIElementRef) -> Option<u32> {
    if let Some(wid) = ax_get_window_id(window) {
        return Some(wid);
    }
    copy_number_attr(window, "AXWindowNumber").and_then(|n| {
        let rounded = n.round();
        if rounded >= 0.0 && rounded <= f64::from(u32::MAX) {
            Some(rounded as u32)
        } else {
            None
        }
    })
}

/// Latest CGWindowList record for the exact layer-0 window, or `None` when it
/// no longer exists.
fn observed_window(pid: i32, window_id: u32) -> Option<WindowInfo> {
    crate::windows::all_windows()
        .into_iter()
        .find(|w| w.pid == pid && w.window_id == window_id && w.layer == 0)
}

/// Poll the observed window until `matches` passes or the deadline elapses.
/// Returns the last observation, or `None` when the window disappeared.
fn wait_for_observed_window(
    pid: i32,
    window_id: u32,
    matches: impl Fn(&WindowInfo) -> bool,
) -> Option<WindowInfo> {
    let deadline = Instant::now() + OBSERVATION_DEADLINE;
    loop {
        let observed = observed_window(pid, window_id)?;
        if matches(&observed) || Instant::now() >= deadline {
            return Some(observed);
        }
        std::thread::sleep(OBSERVATION_POLL);
    }
}

fn bounds_approximately_equal(left: &WindowBounds, right: &WindowBounds) -> bool {
    (left.x - right.x).abs() <= BOUNDS_TOLERANCE
        && (left.y - right.y).abs() <= BOUNDS_TOLERANCE
        && sizes_approximately_equal(left, right)
}

fn sizes_approximately_equal(left: &WindowBounds, right: &WindowBounds) -> bool {
    (left.width - right.width).abs() <= BOUNDS_TOLERANCE
        && (left.height - right.height).abs() <= BOUNDS_TOLERANCE
}

fn format_bounds(bounds: &WindowBounds) -> String {
    format!(
        "x={}, y={}, width={}, height={}",
        bounds.x.round() as i64,
        bounds.y.round() as i64,
        bounds.width.round() as i64,
        bounds.height.round() as i64,
    )
}

fn bounds_json(bounds: &WindowBounds) -> Value {
    serde_json::json!({
        "x": bounds.x,
        "y": bounds.y,
        "width": bounds.width,
        "height": bounds.height,
    })
}

fn require_pid_and_window_id(args: &Value) -> Result<(i32, u32), ToolResult> {
    use cua_driver_core::tool_args::ArgsExt;
    let pid = match args.opt_i64("pid") {
        Some(p) => p,
        None => return Err(ToolResult::error("Missing required integer fields pid and window_id.")),
    };
    let raw_window_id = match args.opt_i64("window_id") {
        Some(w) => w,
        None => return Err(ToolResult::error("Missing required integer fields pid and window_id.")),
    };
    let pid = i32::try_from(pid)
        .map_err(|_| ToolResult::error(format!("pid {pid} is outside the supported Int32 range.")))?;
    let window_id = u32::try_from(raw_window_id).map_err(|_| {
        ToolResult::error(format!("window_id {raw_window_id} is outside the supported UInt32 range."))
    })?;
    Ok((pid, window_id))
}

fn pid_window_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "required": ["pid", "window_id"],
        "properties": {
            "pid": { "type": "integer", "description": "Process ID from `list_windows`." },
            "window_id": { "type": "integer", "description": "CGWindowID from `list_windows`." }
        },
        "additionalProperties": false
    })
}

/// Activate the owning app so the raised window genuinely lands frontmost.
/// Returns `false` when the pid has no running app or Cocoa rejected the swap.
fn activate_app(pid: i32) -> Result<(), String> {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
    let activation = unsafe {
        NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .map(|app| app.activateWithOptions(NSApplicationActivationOptions::empty()))
    };
    match activation {
        None => Err(format!("No running app exists for pid {pid}.")),
        Some(false) => Err(format!("Failed to activate app for pid {pid}.")),
        Some(true) => Ok(()),
    }
}

// ── set_window_bounds ────────────────────────────────────────────────────────

pub struct SetWindowBoundsTool;

static SET_BOUNDS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn set_bounds_def() -> &'static ToolDef {
    SET_BOUNDS_DEF.get_or_init(|| ToolDef {
        name: "set_window_bounds".into(),
        description:
            "Move and resize a top-level app window to exact screen-point bounds.\n\n\
             Use `list_windows` first and pass the target window's `pid` and \
             `window_id`. This tool requires an exact window match; it does not \
             pick another window when the requested one is missing."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["pid", "window_id", "x", "y", "width", "height"],
            "properties": {
                "pid": { "type": "integer", "description": "Process ID from `list_windows`." },
                "window_id": { "type": "integer", "description": "CGWindowID from `list_windows`." },
                "x": { "type": "number", "description": "Left edge in global screen points." },
                "y": { "type": "number", "description": "Top edge in global screen points." },
                "width": { "type": "number", "description": "Window width in screen points." },
                "height": { "type": "number", "description": "Window height in screen points." }
            },
            "additionalProperties": false
        }),
        read_only: false,
        destructive: true,
        idempotent: true,
        open_world: true,
    })
}

#[async_trait]
impl Tool for SetWindowBoundsTool {
    fn def(&self) -> &ToolDef { set_bounds_def() }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let (pid, window_id) = match require_pid_and_window_id(&args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let number = |key: &str| -> Option<f64> {
            args.opt_f64(key).or_else(|| args.opt_i64(key).map(|i| i as f64))
        };
        let (x, y, width, height) = match (number("x"), number("y"), number("width"), number("height")) {
            (Some(x), Some(y), Some(w), Some(h)) => (x, y, w, h),
            _ => return ToolResult::error("Missing required number fields x, y, width, and height."),
        };
        if width <= 0.0 || height <= 0.0 {
            return ToolResult::error("width and height must be greater than 0.");
        }
        let requested = WindowBounds { x, y, width, height };

        let result = tokio::task::spawn_blocking(move || -> Result<WindowInfo, String> {
            let window = find_ax_window(pid, window_id)?;
            let outcome = (|| -> Result<WindowInfo, String> {
                let size_err = unsafe { set_size_attr(window, "AXSize", width, height) };
                if size_err != kAXErrorSuccess {
                    return Err(format!("Failed to set AXSize: {size_err}."));
                }
                let _ = wait_for_observed_window(pid, window_id, |observed| {
                    sizes_approximately_equal(&observed.bounds, &requested)
                });
                let pos_err = unsafe { set_point_attr(window, "AXPosition", x, y) };
                if pos_err != kAXErrorSuccess {
                    return Err(format!("Failed to set AXPosition: {pos_err}."));
                }
                let observed = wait_for_observed_window(pid, window_id, |observed| {
                    bounds_approximately_equal(&observed.bounds, &requested)
                })
                .ok_or_else(|| {
                    format!("Window {window_id} for pid {pid} disappeared after setting bounds.")
                })?;
                if !bounds_approximately_equal(&observed.bounds, &requested) {
                    return Err(format!(
                        "Window {window_id} for pid {pid} did not apply requested bounds. \
                         requested={} observed={}",
                        format_bounds(&requested),
                        format_bounds(&observed.bounds),
                    ));
                }
                Ok(observed)
            })();
            unsafe { CFRelease(window as CFTypeRef); }
            outcome
        })
        .await;

        match result {
            Ok(Ok(observed)) => ToolResult::text(format!(
                "Moved window {window_id} for {} to {}.",
                observed.app_name,
                format_bounds(&observed.bounds),
            ))
            .with_structured(serde_json::json!({
                "pid": pid,
                "window_id": window_id,
                "app_name": observed.app_name,
                "title": observed.title,
                "bounds": bounds_json(&observed.bounds),
            })),
            Ok(Err(e)) => ToolResult::error(e),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── focus_window ─────────────────────────────────────────────────────────────

pub struct FocusWindowTool;

static FOCUS_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn focus_def() -> &'static ToolDef {
    FOCUS_DEF.get_or_init(|| ToolDef {
        name: "focus_window".into(),
        description:
            "Reveal and focus an exact top-level app window.\n\n\
             Use `list_windows` first and pass the target window's `pid` and \
             `window_id`. This tool requires an exact window match; it does \
             not pick another window when the requested one is missing."
            .into(),
        input_schema: pid_window_schema(),
        read_only: false,
        destructive: false,
        idempotent: true,
        open_world: true,
    })
}

/// Observed frontmost window for `pid`: the pid's on-screen layer-0 window
/// with the highest z (CGWindowList is front-to-back; z_index is reversed,
/// so higher = frontmost).
fn frontmost_window_for_pid(pid: i32) -> Option<WindowInfo> {
    crate::windows::visible_windows()
        .into_iter()
        .filter(|w| w.pid == pid && w.layer == 0)
        .max_by_key(|w| w.z_index)
}

#[async_trait]
impl Tool for FocusWindowTool {
    fn def(&self) -> &ToolDef { focus_def() }

    async fn invoke(&self, args: Value) -> ToolResult {
        let (pid, window_id) = match require_pid_and_window_id(&args) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let result = tokio::task::spawn_blocking(move || -> Result<WindowInfo, String> {
            let window = find_ax_window(pid, window_id)?;
            let raise_err = unsafe { perform_action(window, "AXRaise") };
            unsafe { CFRelease(window as CFTypeRef); }
            if raise_err != kAXErrorSuccess {
                return Err(format!(
                    "Failed to raise window {window_id} for pid {pid}: {raise_err}."
                ));
            }
            activate_app(pid)?;

            let deadline = Instant::now() + OBSERVATION_DEADLINE;
            loop {
                if crate::apps::frontmost_pid() == Some(pid) {
                    if let Some(observed) = frontmost_window_for_pid(pid) {
                        if observed.window_id == window_id {
                            return Ok(observed);
                        }
                    }
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "Window {window_id} for pid {pid} did not become focused."
                    ));
                }
                std::thread::sleep(OBSERVATION_POLL);
            }
        })
        .await;

        match result {
            Ok(Ok(observed)) => ToolResult::text(format!(
                "Focused window {window_id} for {}.",
                observed.app_name,
            ))
            .with_structured(serde_json::json!({
                "pid": pid,
                "window_id": window_id,
                "app_name": observed.app_name,
                "title": observed.title,
                "is_focused": true,
                "bounds": bounds_json(&observed.bounds),
            })),
            Ok(Err(e)) => ToolResult::error(e),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── close_window ─────────────────────────────────────────────────────────────

pub struct CloseWindowTool;

static CLOSE_DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn close_def() -> &'static ToolDef {
    CLOSE_DEF.get_or_init(|| ToolDef {
        name: "close_window".into(),
        description:
            "Request-close an exact top-level app window.\n\n\
             Use `list_windows` first and pass the target window's `pid` and \
             `window_id`. This tool presses that window's normal close button \
             and verifies that the same window disappears. It does not kill \
             the process or choose another window."
            .into(),
        input_schema: pid_window_schema(),
        read_only: false,
        destructive: true,
        idempotent: false,
        open_world: true,
    })
}

fn wait_for_window_to_disappear(pid: i32, window_id: u32) -> bool {
    let deadline = Instant::now() + OBSERVATION_DEADLINE;
    loop {
        let still_present = crate::windows::all_windows().into_iter().any(|w| {
            w.pid == pid && w.window_id == window_id && w.layer == 0 && w.is_on_screen
        });
        if !still_present {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(OBSERVATION_POLL);
    }
}

#[async_trait]
impl Tool for CloseWindowTool {
    fn def(&self) -> &ToolDef { close_def() }

    async fn invoke(&self, args: Value) -> ToolResult {
        let (pid, window_id) = match require_pid_and_window_id(&args) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let result = tokio::task::spawn_blocking(move || -> Result<(), String> {
            let window = find_ax_window(pid, window_id)?;
            let outcome = (|| -> Result<(), String> {
                if !crate::input::skylight::activate_without_raise(pid, window_id) {
                    return Err(format!(
                        "Failed to focus window {window_id} for pid {pid} without raising it."
                    ));
                }
                let close_button = unsafe { copy_element_attr(window, "AXCloseButton") }
                    .ok_or_else(|| {
                        format!("Window {window_id} for pid {pid} does not expose a close button.")
                    })?;
                let press_err = unsafe { perform_action(close_button, "AXPress") };
                unsafe { CFRelease(close_button as CFTypeRef); }
                if press_err != kAXErrorSuccess {
                    return Err(format!(
                        "Failed to press close button for window {window_id} pid {pid}: {press_err}."
                    ));
                }
                if !wait_for_window_to_disappear(pid, window_id) {
                    return Err(format!(
                        "Window {window_id} for pid {pid} is still present after close request."
                    ));
                }
                Ok(())
            })();
            unsafe { CFRelease(window as CFTypeRef); }
            outcome
        })
        .await;

        match result {
            Ok(Ok(())) => ToolResult::text(format!("Closed window {window_id} for pid {pid}."))
                .with_structured(serde_json::json!({
                    "pid": pid,
                    "window_id": window_id,
                    "closed": true,
                })),
            Ok(Err(e)) => ToolResult::error(e),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── minimize_window / restore_window / maximize_window ──────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum LifecycleAction {
    Minimize,
    Restore,
    Maximize,
}

impl LifecycleAction {
    fn tool_name(self) -> &'static str {
        match self {
            Self::Minimize => "minimize_window",
            Self::Restore => "restore_window",
            Self::Maximize => "maximize_window",
        }
    }

    fn summary_line(self) -> &'static str {
        match self {
            Self::Minimize => "Minimize an exact top-level app window into the Dock.",
            Self::Restore => "Restore an exact minimized top-level app window and focus it.",
            Self::Maximize => "Zoom/maximize an exact top-level app window with the standard window zoom button.",
        }
    }

    fn summary_verb(self) -> &'static str {
        match self {
            Self::Minimize => "Minimized",
            Self::Restore => "Restored",
            Self::Maximize => "Maximized",
        }
    }
}

pub struct WindowLifecycleTool {
    action: LifecycleAction,
    def: ToolDef,
}

impl WindowLifecycleTool {
    fn new(action: LifecycleAction) -> Self {
        let def = ToolDef {
            name: action.tool_name().into(),
            description: format!(
                "{}\n\n\
                 Use `list_windows` first and pass the target window's `pid` and \
                 `window_id`. This tool requires an exact window match; it does \
                 not pick another window when the requested one is missing.",
                action.summary_line(),
            ),
            input_schema: pid_window_schema(),
            read_only: false,
            destructive: false,
            idempotent: action != LifecycleAction::Maximize,
            open_world: true,
        };
        Self { action, def }
    }

    pub fn minimize() -> Self { Self::new(LifecycleAction::Minimize) }
    pub fn restore() -> Self { Self::new(LifecycleAction::Restore) }
    pub fn maximize() -> Self { Self::new(LifecycleAction::Maximize) }
}

fn perform_lifecycle(
    action: LifecycleAction,
    pid: i32,
    window_id: u32,
) -> Result<WindowInfo, String> {
    let window = find_ax_window(pid, window_id)?;
    let outcome = (|| -> Result<WindowInfo, String> {
        match action {
            LifecycleAction::Minimize => {
                let err = unsafe { set_bool_attr(window, "AXMinimized", true) };
                if err != kAXErrorSuccess {
                    return Err(format!("Failed to set AXMinimized: {err}."));
                }
                wait_for_observed_window(pid, window_id, |observed| !observed.is_on_screen)
                    .ok_or_else(|| format!(
                        "Window {window_id} for pid {pid} disappeared after minimize request."
                    ))
            }
            LifecycleAction::Restore => {
                let err = unsafe { set_bool_attr(window, "AXMinimized", false) };
                if err != kAXErrorSuccess {
                    return Err(format!("Failed to set AXMinimized: {err}."));
                }
                activate_app(pid)?;
                let _ = unsafe { perform_action(window, "AXRaise") };
                let observed = wait_for_observed_window(pid, window_id, |observed| observed.is_on_screen)
                    .ok_or_else(|| format!(
                        "Window {window_id} for pid {pid} did not restore on screen."
                    ))?;
                if !observed.is_on_screen {
                    return Err(format!(
                        "Window {window_id} for pid {pid} did not restore on screen."
                    ));
                }
                Ok(observed)
            }
            LifecycleAction::Maximize => {
                let zoom_button = unsafe { copy_element_attr(window, "AXZoomButton") }
                    .ok_or_else(|| "AXZoomButton is unavailable on the requested window.".to_owned())?;
                let press_err = unsafe { perform_action(zoom_button, "AXPress") };
                unsafe { CFRelease(zoom_button as CFTypeRef); }
                if press_err != kAXErrorSuccess {
                    return Err(format!("Failed to press AXZoomButton: {press_err}."));
                }
                wait_for_observed_window(pid, window_id, |_| true).ok_or_else(|| format!(
                    "Window {window_id} for pid {pid} disappeared after maximize request."
                ))
            }
        }
    })();
    unsafe { CFRelease(window as CFTypeRef); }
    outcome
}

#[async_trait]
impl Tool for WindowLifecycleTool {
    fn def(&self) -> &ToolDef { &self.def }

    async fn invoke(&self, args: Value) -> ToolResult {
        let (pid, window_id) = match require_pid_and_window_id(&args) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let action = self.action;

        let result = tokio::task::spawn_blocking(move || {
            perform_lifecycle(action, pid, window_id)
        })
        .await;

        match result {
            Ok(Ok(observed)) => ToolResult::text(format!(
                "{} window {window_id} for {}.",
                action.summary_verb(),
                observed.app_name,
            ))
            .with_structured(serde_json::json!({
                "action": action.tool_name(),
                "pid": pid,
                "window_id": window_id,
                "app_name": observed.app_name,
                "title": observed.title,
                "is_on_screen": observed.is_on_screen,
                "bounds": bounds_json(&observed.bounds),
            })),
            Ok(Err(e)) => ToolResult::error(e),
            Err(e) => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_and_schemas_match_contract() {
        assert_eq!(set_bounds_def().name, "set_window_bounds");
        assert_eq!(focus_def().name, "focus_window");
        assert_eq!(close_def().name, "close_window");
        assert_eq!(WindowLifecycleTool::minimize().def.name, "minimize_window");
        assert_eq!(WindowLifecycleTool::restore().def.name, "restore_window");
        assert_eq!(WindowLifecycleTool::maximize().def.name, "maximize_window");

        for def in [set_bounds_def(), focus_def(), close_def()] {
            let required: Vec<&str> = def.input_schema["required"]
                .as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
            assert!(required.contains(&"pid"));
            assert!(required.contains(&"window_id"));
        }
        let bounds_required: Vec<&str> = set_bounds_def().input_schema["required"]
            .as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
        for field in ["x", "y", "width", "height"] {
            assert!(bounds_required.contains(&field));
        }
    }

    #[test]
    fn missing_args_fail_loudly() {
        let err = require_pid_and_window_id(&serde_json::json!({ "pid": 42 })).unwrap_err();
        assert_eq!(err.is_error, Some(true));
        let err = require_pid_and_window_id(&serde_json::json!({ "window_id": 42 })).unwrap_err();
        assert_eq!(err.is_error, Some(true));
        let err = require_pid_and_window_id(&serde_json::json!({ "pid": 42, "window_id": -3 })).unwrap_err();
        assert_eq!(err.is_error, Some(true));
        let ok = require_pid_and_window_id(&serde_json::json!({ "pid": 42, "window_id": 7 })).unwrap();
        assert_eq!(ok, (42, 7));
    }
}
