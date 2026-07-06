//! set_value tool — matches the Swift reference in SetValueTool.swift.
//!
//! Two modes, determined by the element's AXRole:
//!
//! * **AXPopUpButton**: Find the child option whose AXTitle or AXValue matches
//!   `value` (case-insensitive) and AXPress it directly.  The native macOS popup
//!   menu is never opened, so focus is never stolen.  Falls back to Safari
//!   `osascript do JavaScript` for WebKit `<select>` elements that expose no AX
//!   children when the popup is closed.
//!
//! * **Everything else**: Write `AXValue` directly (sliders, steppers, native
//!   text fields that expose a settable AXValue).

use async_trait::async_trait;
use cua_driver_core::{protocol::ToolResult, tool::{Tool, ToolDef}};
use serde_json::Value;
use std::sync::Arc;

use crate::apps;
use crate::ax::bindings::{
    copy_children, copy_number_attr, copy_string_attr, perform_action, set_number_attr,
    set_string_attr, kAXErrorSuccess, AXUIElementRef,
};
use crate::focus_guard;
use crate::window_change_detector::WindowChangeDetector;
use core_foundation::base::CFRelease;

use super::ToolState;

pub struct SetValueTool {
    state: Arc<ToolState>,
}

impl SetValueTool {
    pub fn new(state: Arc<ToolState>) -> Self { Self { state } }
}

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "set_value".into(),
        description:
            "Set a value on a UI element. Two modes depending on element role:\n\
             \n\
             - **AXPopUpButton / select dropdown**: finds the child option whose \
             title or value matches `value` (case-insensitive) and AXPresses it \
             directly — the native macOS popup menu is never opened, so focus \
             is never stolen. Use this for HTML <select> elements in Safari or \
             any native NSPopUpButton.\n\
             \n\
             - **All other elements**: writes AXValue directly (sliders, steppers, \
             date pickers, native text fields that expose settable AXValue).\n\
             \n\
             For free-form text entry into web inputs, prefer `type_text_chars` \
             which synthesises key events — AXValue writes are ignored by WebKit."
            .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "required": ["pid", "value"],
            "properties": {
                "session": { "type": "string", "description": "Optional session id: declares/uses the agent cursor and per-session state for this run. The same id works over MCP, the CLI, or the raw socket, and follows the run across apps/windows. Omit to run cursor-less." },
                "pid": { "type": "integer" },
                "window_id": {
                    "type": "integer",
                    "description": "CGWindowID for the window whose get_window_state produced the element_index. Required when element_index is used; optional when element_token is supplied (the token carries it)."
                },
                "element_index": { "type": "integer", "description": "Element index from last get_window_state. Must be supplied unless element_token is provided. REQUIRES `pid` and `window_id` to be passed alongside it — element_index alone (no pid) fails fast with \"Missing required integer field: pid\"; it is not a silent no-op." },
                "element_token": { "type": "string",  "description": "Opaque per-snapshot element handle from `structuredContent.elements[].element_token`. Takes precedence over element_index when both supplied. Returns an explicit \"stale\" error if the snapshot has been superseded." },
                "value": {
                    "type": "string",
                    "description": "New value. AX will coerce to the element's native type."
                }
            },
            "additionalProperties": false
        }),
        read_only:   false,
        destructive: true,
        idempotent:  true,
        open_world:  true,
    })
}

#[async_trait]
impl Tool for SetValueTool {
    fn def(&self) -> &ToolDef { def() }

    async fn invoke(&self, args: Value) -> ToolResult {
        use cua_driver_core::tool_args::ArgsExt;
        let pid = match args.require_i32("pid") { Ok(v) => v, Err(e) => return e };
        let value = match args.require_str("value") { Ok(v) => v, Err(e) => return e };

        // Surface 6: element_token / element_index precedence. Neither
        // is now schema-required so the resolver can centralize the
        // "missing addressing" error message.
        let element_token_arg = args.opt_str("element_token");
        let window_id_arg     = args.opt_u64("window_id").map(|v| v as u32);
        let element_index_arg = args.opt_u64("element_index").map(|v| v as usize);
        let resolved = match cua_driver_core::element_token::resolve_element_args(
            pid,
            element_index_arg,
            element_token_arg.as_deref(),
            window_id_arg,
            "set_value",
        ) {
            Ok(r) => r,
            Err(e) => return e,
        };
        let (element_index, window_id) = match resolved {
            cua_driver_core::element_token::ResolvedElement::None =>
                return ToolResult::error(
                    "set_value requires element_index (+ window_id) or element_token to \
                     address the target element."
                ),
            cua_driver_core::element_token::ResolvedElement::Element {
                window_id: Some(wid), element_index: idx, via_token: _,
            } => (idx, wid),
            cua_driver_core::element_token::ResolvedElement::Element {
                window_id: None, ..
            } => return ToolResult::error(
                "set_value requires window_id when element_index is used \
                 (omit only when supplying element_token, which carries it)."
            ),
        };

        // Retain out of the cache so a concurrent get_window_state can't free
        // the element mid-action (use-after-free → daemon crash). Guard lives
        // to the end of this method, past the AX write below.
        let element_guard = match self.state.element_cache.get_element_retained(pid, window_id, element_index) {
            Some(e) => e,
            None => return ToolResult::error(format!(
                "Element index {element_index} not found. Call get_window_state first."
            )),
        };
        let element_ptr = element_guard.as_ptr();

        // ── Focus-suppression wrap (Swift WindowChangeDetector + FocusGuard) ──
        // AXValue writes on popups / sliders can cause reflex activations
        // in Chromium-based apps; the AXPopUpButton path also AXPresses a
        // child option which can trigger app activation in some setups.
        let prior_front = apps::frontmost_pid();
        let snapshot = WindowChangeDetector::snapshot(prior_front);

        let result = focus_guard::with_focus_suppressed(
            Some(pid),
            prior_front,
            "set_value.AXValue",
            || async move {
                tokio::task::spawn_blocking(move || {
                    set_value_blocking(element_ptr, element_index, pid, &value)
                })
                .await
            },
        )
        .await;

        let changes = snapshot.detect_async().await;

        match result {
            Ok(Ok(mut msg)) => {
                msg.push_str(&changes.result_suffix());
                ToolResult::text(msg)
            }
            Ok(Err(e))   => ToolResult::error(format!("set_value failed: {e}")),
            Err(e)       => ToolResult::error(format!("Task error: {e}")),
        }
    }
}

// ── Blocking implementation (runs on spawn_blocking thread) ─────────────────

fn set_value_blocking(
    element_ptr: usize,
    element_index: usize,
    pid: i32,
    value: &str,
) -> anyhow::Result<String> {
    let element = element_ptr as AXUIElementRef;

    let role = unsafe { copy_string_attr(element, "AXRole") }
        .unwrap_or_default();

    if role == "AXPopUpButton" {
        let element_title = unsafe { copy_string_attr(element, "AXTitle") }
            .unwrap_or_default();
        select_popup_option(element, element_index, pid, value, &element_title)
    } else {
        // Default path: write AXValue directly. Numeric controls (AXSlider /
        // AXStepper) reject a CFString with -25201 and need a CFNumber; text
        // fields take a CFString. Try numeric first when the value parses as a
        // number, then fall back to a string write.
        // Numeric target carried through so we can step toward it if the
        // direct writes are rejected (SwiftUI AXSlider rejects every AXValue
        // write with -25200 yet exposes a readable AXValue + increment/decrement
        // actions).
        let numeric_target = value.trim().parse::<f64>().ok();
        let err = match numeric_target {
            Some(n) => {
                let e = unsafe { set_number_attr(element, "AXValue", n) };
                if e == kAXErrorSuccess {
                    e
                } else {
                    unsafe { set_string_attr(element, "AXValue", value) }
                }
            }
            None => unsafe { set_string_attr(element, "AXValue", value) },
        };
        if err == kAXErrorSuccess {
            Ok(format!("✅ Set AXValue on [{element_index}] {role}."))
        } else if let Some(target) = numeric_target {
            // Both direct writes failed for a numeric target — fall back to
            // stepping the control via AXIncrement / AXDecrement actions.
            if step_to_value(element, target) {
                Ok(format!(
                    "✅ Set AXValue on [{element_index}] {role} via AXIncrement/AXDecrement stepping."
                ))
            } else {
                anyhow::bail!("AXUIElementSetAttributeValue(AXValue) failed with error {err}")
            }
        } else {
            anyhow::bail!("AXUIElementSetAttributeValue(AXValue) failed with error {err}")
        }
    }
}

// ── AXIncrement / AXDecrement stepping fallback ──────────────────────────────

/// Step a numeric control toward `target` using its `AXIncrement` /
/// `AXDecrement` actions. Used only when direct `AXValue` writes are rejected
/// (notably SwiftUI's `AXSlider`, which exposes a readable-but-unsettable
/// `AXValue` plus increment/decrement actions).
///
/// Returns `true` once the control's value lands within half of the last
/// observed step of `target`, `false` if it can't be read or can't be moved.
fn step_to_value(element: AXUIElementRef, target: f64) -> bool {
    // Can't target precisely without feedback — bail if AXValue is unreadable.
    let mut current = match unsafe { copy_number_attr(element, "AXValue") } {
        Some(v) => v,
        None => return false,
    };

    // Half of the last observed step. Start near-zero so we never declare the
    // target "reached" before performing (and observing) a real
    // AXIncrement/AXDecrement — otherwise a slider at 0.0 targeting 0.5 would
    // report success without ever moving. The radius widens only after we learn
    // the control's actual step size from an observed value change.
    let mut step_radius = f64::EPSILON;

    // Hard cap to prevent runaway on a control that never quite converges.
    for _ in 0..500 {
        if (current - target).abs() <= step_radius {
            return true;
        }

        let action = if current < target { "AXIncrement" } else { "AXDecrement" };
        let _ = unsafe { perform_action(element, action) };

        let next = match unsafe { copy_number_attr(element, "AXValue") } {
            Some(v) => v,
            None => return false,
        };

        // The action didn't move the value — the control can't be stepped (or
        // has hit a min/max bound short of target). Stop to avoid looping.
        if next == current {
            return false;
        }

        // Refine the stop threshold to half of the actual step the control took.
        let step = (next - current).abs();
        if step > 0.0 {
            step_radius = step / 2.0;
        }
        current = next;
    }

    // Exhausted the iteration cap without converging.
    (current - target).abs() <= step_radius
}

// ── AXPopUpButton path ───────────────────────────────────────────────────────

fn select_popup_option(
    element: AXUIElementRef,
    element_index: usize,
    pid: i32,
    value: &str,
    element_title: &str,
) -> anyhow::Result<String> {
    // ── Strategy 1: AX children (native AppKit popup buttons) ──────────────
    // Chromium-family popup buttons wrap their AXMenuItem options in a
    // single empty-titled AXMenu/AXGroup child, and expose option text on
    // AXValue or AXDescription instead of AXTitle. Flatten one container
    // level and read the observed text attributes in order — raw
    // observation only, no inference.
    let children = flatten_popup_option_candidates(unsafe { copy_children(element) }, 0);

    if !children.is_empty() {
        let value_lower = value.to_lowercase();
        let mut matched_idx: Option<usize> = None;
        let mut available: Vec<String> = Vec::with_capacity(children.len());

        for (i, &child) in children.iter().enumerate() {
            let child_text = popup_option_text(child);
            available.push(child_text.clone());
            if matched_idx.is_none() && child_text.to_lowercase() == value_lower {
                matched_idx = Some(i);
            }
        }

        let result = if let Some(i) = matched_idx {
            let child = children[i];
            let observed_title = popup_option_text(child);
            let opt_title = if observed_title.is_empty() { value.to_string() } else { observed_title };
            let err = unsafe { perform_action(child, "AXPress") };
            if err == kAXErrorSuccess {
                Ok(format!(
                    "✅ Selected '{opt_title}' in AXPopUpButton [{element_index}] \
                     \"{element_title}\" via AX child AXPress."
                ))
            } else {
                Err(anyhow::anyhow!(
                    "AXPress on child option failed with error {err}"
                ))
            }
        } else {
            let avail = available.iter()
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow::anyhow!(
                "No AX child matching '{value}' in AXPopUpButton [{element_index}] \
                 \"{element_title}\". Available: [{avail}]"
            ))
        };

        // Release children (flatten transfers a retain for each returned ref).
        for &child in &children {
            unsafe { CFRelease(child as _); }
        }

        return result;
    }

    // ── Strategy 1b: open-then-pick (Chromium-family popup buttons) ─────────
    // Chromium <select> elements expose no AX children while closed. Press
    // the popup to materialize its menu, re-read the flattened candidates,
    // press the matching option, and verify the popup's observed AXValue.
    // All raw observation; the menu is cancelled on failure so the page is
    // left untouched. Returns None only when the popup never materialized
    // children, so the Safari JS fallback below still applies.
    if let Some(picked) = open_and_pick_popup_option(element, element_index, value, element_title) {
        return picked;
    }

    // Strategy 2: Safari/WebKit — no AX children when popup is closed.
    // Use osascript do JavaScript to set the <select> element's DOM value.
    let app_name = crate::apps::get_app_name_for_pid(pid)
        .unwrap_or_default();

    if app_name != "Safari" {
        anyhow::bail!(
            "AXPopUpButton [{element_index}] '{element_title}' has no AX children and \
             target is '{app_name}' (not Safari) — no fallback available."
        )
    }

    set_select_via_js(element_index, element_title, value)
}

/// Observed option text for a popup option candidate: first non-empty of
/// AXTitle, AXValue, AXDescription. Raw attribute reads only.
fn popup_option_text(element: AXUIElementRef) -> String {
    for attribute in ["AXTitle", "AXValue", "AXDescription"] {
        if let Some(text) = unsafe { copy_string_attr(element, attribute) } {
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

/// Flatten popup option candidates by descending into text-less container
/// children (AXMenu/AXGroup/AXList) up to two levels, so Chromium-style
/// AXPopUpButton > AXMenu > AXMenuItem shapes expose their real options.
///
/// Ownership: takes retained refs (as returned by `copy_children`) and
/// returns retained refs. Containers that are descended into are released
/// here; every returned element carries one retain the caller must release.
fn flatten_popup_option_candidates(
    elements: Vec<AXUIElementRef>,
    depth: usize,
) -> Vec<AXUIElementRef> {
    let mut result: Vec<AXUIElementRef> = Vec::new();
    for element in elements {
        let role = unsafe { copy_string_attr(element, "AXRole") }.unwrap_or_default();
        let is_container = role == "AXMenu" || role == "AXGroup" || role == "AXList";
        if depth < 2 && (is_container || popup_option_text(element).is_empty()) {
            let children = unsafe { copy_children(element) };
            if !children.is_empty() {
                result.extend(flatten_popup_option_candidates(children, depth + 1));
                unsafe { CFRelease(element as _); }
                continue;
            }
        }
        result.push(element);
    }
    result
}

/// Open a closed popup, pick the matching option from the materialized menu,
/// verify the popup's observed AXValue, and cancel the menu when no option
/// matches. Returns None when the popup never materialized children, so the
/// caller can fall through to the next strategy.
fn open_and_pick_popup_option(
    element: AXUIElementRef,
    element_index: usize,
    value: &str,
    element_title: &str,
) -> Option<anyhow::Result<String>> {
    let press_err = unsafe { perform_action(element, "AXPress") };
    if press_err != kAXErrorSuccess {
        return Some(Err(anyhow::anyhow!(
            "AXPress to open AXPopUpButton [{element_index}] \"{element_title}\" \
             failed with error {press_err}"
        )));
    }
    std::thread::sleep(std::time::Duration::from_millis(250));

    let opened = flatten_popup_option_candidates(unsafe { copy_children(element) }, 0);
    if opened.is_empty() {
        let _ = unsafe { perform_action(element, "AXCancel") };
        return None;
    }

    let value_lower = value.to_lowercase();
    let mut available: Vec<String> = Vec::with_capacity(opened.len());
    let mut matched: Option<AXUIElementRef> = None;
    for &candidate in &opened {
        let text = popup_option_text(candidate);
        if matched.is_none() && text.to_lowercase() == value_lower {
            matched = Some(candidate);
        }
        available.push(text);
    }

    let result = match matched {
        None => {
            let _ = unsafe { perform_action(element, "AXCancel") };
            let avail = available.iter()
                .map(|t| format!("\"{t}\""))
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow::anyhow!(
                "❌ No exposed option matching '{value}' in opened AXPopUpButton \
                 [{element_index}] \"{element_title}\". Available: [{avail}]."
            ))
        }
        Some(option) => {
            let press_err = unsafe { perform_action(option, "AXPress") };
            if press_err != kAXErrorSuccess {
                let _ = unsafe { perform_action(element, "AXCancel") };
                Err(anyhow::anyhow!(
                    "AXPress on opened option '{value}' failed with error {press_err}"
                ))
            } else {
                std::thread::sleep(std::time::Duration::from_millis(150));
                let observed_value = unsafe { copy_string_attr(element, "AXValue") }
                    .unwrap_or_default();
                if observed_value.to_lowercase() == value_lower {
                    Ok(format!(
                        "✅ Selected '{value}' in AXPopUpButton [{element_index}] \
                         \"{element_title}\" via open-then-pick; popup AXValue now \
                         \"{observed_value}\"."
                    ))
                } else {
                    Err(anyhow::anyhow!(
                        "❌ Pressed option '{value}' in AXPopUpButton [{element_index}] \
                         \"{element_title}\" but observed AXValue is \"{observed_value}\". \
                         Reread app state and verify the visible selection."
                    ))
                }
            }
        }
    };

    for &candidate in &opened {
        unsafe { CFRelease(candidate as _); }
    }

    Some(result)
}

// ── Safari JavaScript fallback ───────────────────────────────────────────────

/// Set an HTML `<select>` value in Safari via `osascript do JavaScript`.
/// Searches all `<select>` elements for an `<option>` whose text or value matches
/// `value` (case-insensitive), then sets it and dispatches a `change` event.
fn set_select_via_js(
    element_index: usize,
    element_title: &str,
    value: &str,
) -> anyhow::Result<String> {
    // Percent-encode the lowercased value using only unreserved URL characters
    // as the allowed set, matching the Swift reference's percent-encoding approach.
    // This makes the string safe to embed in both a JS single-quoted string
    // (via decodeURIComponent) and an AppleScript double-quoted string.
    let v_low = value.to_lowercase();
    let v_encoded = percent_encode_unreserved(&v_low);

    // JavaScript that matches the Swift reference verbatim.
    let js = format!(
        "(function(){{\
         var v=decodeURIComponent('{v_encoded}');\
         var ss=document.querySelectorAll('select'),opts=[];\
         for(var i=0;i<ss.length;i++){{\
         for(var j=0;j<ss[i].options.length;j++){{\
         var t=ss[i].options[j].text.toLowerCase(),\
         u=ss[i].options[j].value.toLowerCase();\
         opts.push(t+'|'+u);\
         if(t===v||u===v){{\
         ss[i].value=ss[i].options[j].value;\
         ss[i].dispatchEvent(new Event('change',{{bubbles:true}}));\
         return 'SET:'+ss[i].value;}}}}\
         }}return 'NOTFOUND:'+opts.join(',');\
         }})()"
    );

    let apple_script = format!(
        "tell application \"Safari\" to do JavaScript \"{js}\" in front document"
    );

    // Spawn osascript with a 10-second deadline. A stuck Safari permission
    // prompt or unresponsive renderer can cause wait() to block indefinitely,
    // which would stall the MCP tool handler permanently.
    let mut child = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&apple_script)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("osascript launch failed: {e}"))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    anyhow::bail!("osascript timed out after 10 seconds");
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => anyhow::bail!("osascript wait error: {e}"),
        }
    }
    let out = child.wait_with_output()
        .map_err(|e| anyhow::anyhow!("osascript output error: {e}"))?;

    let raw = String::from_utf8_lossy(&out.stdout)
        .trim()
        .to_string();

    if raw.starts_with("SET:") {
        let dom_val = &raw[4..];
        Ok(format!(
            "✅ Set select [{element_index}] '{element_title}' to '{value}' via \
             Safari JavaScript (DOM value: \"{dom_val}\")."
        ))
    } else if raw.starts_with("NOTFOUND:") {
        let available = &raw[9..];
        anyhow::bail!(
            "No <option> matching '{value}' found in any <select>. \
             Available (text|value): {available}"
        )
    } else if raw.is_empty() && !out.status.success() {
        let err_text = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("osascript failed: {}", err_text.trim())
    } else {
        anyhow::bail!(
            "JavaScript returned unexpected output: {}",
            &raw[..raw.len().min(200)]
        )
    }
}

// ── Percent-encoding helper ──────────────────────────────────────────────────

/// Percent-encode a string, leaving only unreserved URL characters (`-._~` +
/// alphanumerics) unencoded.  Matches the Swift reference's approach.
fn percent_encode_unreserved(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_' || b == b'~' {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0xF));
        }
    }
    out
}

fn hex_digit(n: u8) -> char {
    match n {
        0..=9  => (b'0' + n) as char,
        10..=15 => (b'A' + n - 10) as char,
        _      => '0',
    }
}
