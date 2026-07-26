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
    copy_bool_attr, copy_children, copy_number_attr, copy_range_attr, copy_string_attr,
    perform_action, set_number_attr, set_range_attr, set_string_attr, kAXErrorSuccess,
    AXUIElementRef,
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
                },
                "skip_change_detection": {
                    "type": "boolean",
                    "description": "Skip the ~1s post-action window/foreground side-effect poll. For batch executors that reread state after the whole batch. Focus-steal suppression still covers the action itself."
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
        // Batch executors that reread state after the whole batch can opt out
        // of the ~1s post-action side-effect poll (Swift reference parity with
        // click's skip_change_detection).
        let skip_change_detection = args.bool_or("skip_change_detection", false);

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

        // Dropping the snapshot without detecting still releases the wildcard
        // suppression lease that covered the action window.
        let changes = if skip_change_detection {
            drop(snapshot);
            crate::window_change_detector::Changes::no_change()
        } else {
            snapshot.detect_async().await
        };

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
    } else if super::type_text::target_in_web_area(pid, Some((element_ptr, Some(element_index)))) {
        // Web content (an AXWebArea ancestor): Chromium/WebKit acknowledge an
        // AXValue write while the renderer/DOM never observes it, so a bare
        // AXValue write here is a silent no-op that used to report "✅ Set
        // AXValue". Route through the same verified keystroke replace ladder
        // type_text uses: AX-focus the element, Cmd+A + Delete, synthesize the
        // characters, then confirm by AXValue read-back.
        keystroke_replace_verified(element_ptr, element_index, pid, value, &role)
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

// ── Verified keystroke replace (web content) ─────────────────────────────────

/// Replace a web-content text field's value with `value` via synthesized
/// keystrokes, verifying every asynchronous step by observed read-back.
///
/// Chromium/WebKit process synthesized editing keystrokes asynchronously, and
/// a background Chromium window drops Cmd+A entirely (it is a menu key
/// equivalent, which the authenticated post-to-pid path bypasses): the
/// instrumented-form trace showed fire-and-forget Cmd+A+Delete clearing either
/// swallowing the first typed character (late select-all) or never applying at
/// all (back-to-back replaces concatenated and the Delete removed one trailing
/// character). So:
///
/// 1. AX-focus the element (fire-and-forget: `AXFocused` is not reliably
///    readable on Chromium text nodes, so the strict final read-back is the
///    authority on whether typing reached the right field).
/// 2. If the field observably has content: write `AXSelectedTextRange`
///    covering the whole value (a real renderer selection change, unlike
///    AXValue writes) and poll the range read-back until it is observably
///    applied. The first typed character then replaces the selection —
///    editing's atomic replace — so no separate Delete keystroke can race
///    the typed text.
/// 3. Type the characters (10ms inter-char delay — safe because the final
///    read-back is strict; target-side instrumentation shows Chromium applies
///    them in order at posting pace).
/// 4. Poll AXValue until it contains `value`. Chromium's a11y serialization
///    of a background window can lag the DOM by over a second, so the ceiling
///    is generous; the poll returns as soon as the value is observed. Replace
///    semantics demand this strict check; a "field grew" heuristic would mask
///    dropped characters.
fn keystroke_replace_verified(
    element_ptr: usize,
    element_index: usize,
    pid: i32,
    value: &str,
    role: &str,
) -> anyhow::Result<String> {
    const SELECT_CONFIRM_TIMEOUT_MS: u64 = 1000;
    const READBACK_CONFIRM_TIMEOUT_MS: u64 = 3000;
    const POLL_INTERVAL_MS: u64 = 15;

    const FOCUS_CONFIRM_TIMEOUT_MS: u64 = 1000;

    let element = element_ptr as AXUIElementRef;
    let target = Some((element_ptr, Some(element_index)));
    let before = super::type_text::read_axvalue(pid, target);
    let _ = crate::input::ax_actions::focus_element(element_ptr);

    // Confirm the focus write observably landed before synthesizing any
    // keystrokes. Typing immediately after the AXFocused write races the
    // renderer's focus handoff and drops leading characters (observed on
    // Chromium: requested "M54.16 ..." read back as "54.16 ..."). When the
    // element exposes AXFocused, require it to become true within the
    // deadline and fail loudly otherwise; when the attribute is never
    // readable there is no evidence channel and the ladder proceeds to the
    // strict read-back below, which still decides the outcome.
    {
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(FOCUS_CONFIRM_TIMEOUT_MS);
        let mut attribute_readable = false;
        loop {
            match unsafe { copy_bool_attr(element, "AXFocused") } {
                Some(true) => break,
                Some(false) => {
                    attribute_readable = true;
                }
                None => {}
            }
            if std::time::Instant::now() >= deadline {
                if attribute_readable {
                    anyhow::bail!(
                        "set_value on [{element_index}] {role} could not confirm keyboard focus \
                         before typing (AXFocused stayed false). Reread window state and retry."
                    );
                }
                break;
            }
            // Re-issue the idempotent focus write in case the first write
            // raced the renderer's focus handoff.
            let _ = crate::input::ax_actions::focus_element(element_ptr);
            std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
        }
    }

    // Bounded re-driven verified replace. The whole operation is "replace the
    // field content with `value`", which is idempotent from any intermediate
    // state, so when the strict read-back shows the typed value did not land
    // (Chromium drops synthesized keystrokes entirely for a beat after
    // popup-menu teardown, and can drop leading characters under load) the
    // select-and-retype is re-driven within the overall deadline instead of
    // trusting a single one-shot attempt. The final read-back still decides
    // the outcome loudly; nothing is reported that was not observed.
    const ATTEMPT_READBACK_WINDOW_MS: u64 = 800;
    let caret_end = value.chars().count() as core_foundation::base::CFIndex;
    let overall_deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(READBACK_CONFIRM_TIMEOUT_MS);
    let mut last_observed: Option<String> = before.clone();
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        if attempt > 1 {
            // The previous verified attempt observably landed nothing.
            // Chromium routes synthesized keystrokes to a dead popup-menu
            // widget after menu teardown until a real element activation
            // resets key routing; an AXFocused write alone does not (observed
            // live: typing swallowed for 3s after a popup pick, landing in
            // 0.3s after an AXPress on the field). Re-drive through the same
            // activation primitive the click tool uses, then re-confirm
            // observed focus below via the selection write.
            let _ = unsafe { perform_action(element, "AXPress") };
            std::thread::sleep(std::time::Duration::from_millis(50));
            let _ = crate::input::ax_actions::focus_element(element_ptr);
        }
        // Select-all only when the field observably has content. An empty
        // field needs no clear, and an unreadable one gives no evidence to
        // verify a selection against — the strict read-back below still
        // decides the outcome.
        let current = super::type_text::read_axvalue(pid, target);
        if matches!(current.as_deref(), Some(b) if !b.is_empty()) {
            let select_len = current.as_deref().map_or(0, |b| b.chars().count()) as core_foundation::base::CFIndex;
            // Select the whole value and wait for the selection to be
            // observably applied. The write is idempotent, so it is re-issued
            // each poll round in case an early write raced the renderer's
            // focus handoff.
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_millis(SELECT_CONFIRM_TIMEOUT_MS);
            loop {
                let err = unsafe { set_range_attr(element, "AXSelectedTextRange", 0, select_len) };
                if err != kAXErrorSuccess {
                    anyhow::bail!(
                        "set_value on [{element_index}] {role} could not select the existing text \
                         for replacement (AXSelectedTextRange write failed with error {err}). \
                         Reread window state and retry."
                    );
                }
                let observed = unsafe { copy_range_attr(element, "AXSelectedTextRange") };
                if matches!(observed, Some((0, len)) if len >= select_len) {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    anyhow::bail!(
                        "set_value on [{element_index}] {role} could not confirm the text \
                         selection before retyping (observed range {observed:?}). Reread window \
                         state and retry."
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
            }
        }

        dismiss_open_popup_menu_window(pid);
        crate::input::keyboard::type_text_with_delay(pid, value, 10)?;

        // Strict replace read-back, polled to cover Chromium's a11y update
        // lag. Chromium coalesces value updates for the focused editing node
        // and can hold the flushed AXValue at a mid-typing snapshot
        // indefinitely; an idempotent caret-to-end selection write (the caret
        // position typing already left) nudges the renderer's a11y pipeline
        // to flush without changing any user-visible state. Returns as soon
        // as the typed value is observed.
        let attempt_deadline = std::cmp::min(
            std::time::Instant::now()
                + std::time::Duration::from_millis(ATTEMPT_READBACK_WINDOW_MS),
            overall_deadline,
        );
        loop {
            let after = super::type_text::read_axvalue(pid, target);
            if matches!(after.as_deref(), Some(a) if a.contains(value)) {
                return Ok(format!(
                    "✅ Set value on [{element_index}] {role} via verified keystroke replace \
                     (web content ignores AX value writes)."
                ));
            }
            last_observed = after;
            if std::time::Instant::now() >= attempt_deadline {
                break;
            }
            let _ = unsafe { set_range_attr(element, "AXSelectedTextRange", caret_end, 0) };
            std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
        }

        if std::time::Instant::now() >= overall_deadline {
            anyhow::bail!(
                "set_value on [{element_index}] {role} did not land: the keystroke replace \
                 read-back observed {last_observed:?} instead of the requested value. Reread \
                 window state and retry."
            );
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
                // Verify the press landed instead of trusting AXPress alone:
                // Chromium keeps previously materialized menu items cached in
                // the closed popup's AX subtree, and pressing a cached item
                // does not reliably select it. Require the popup's observed
                // AXValue to become the requested option (closing any menu
                // the press opened first); otherwise fall through to
                // open-then-pick, which verifies the committed state.
                let value_lower_verify = value.to_lowercase();
                let child_verify_deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(400);
                let observed = loop {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    let observed = unsafe { copy_string_attr(element, "AXValue") }
                        .unwrap_or_default();
                    if observed.to_lowercase() == value_lower_verify
                        || std::time::Instant::now() >= child_verify_deadline
                    {
                        break observed;
                    }
                };
                close_popup_menu(element, pid);
                let settled = unsafe { copy_string_attr(element, "AXValue") }
                    .unwrap_or_default();
                if observed.to_lowercase() == value_lower_verify
                    && settled.to_lowercase() == value_lower_verify
                {
                    Some(Ok(format!(
                        "✅ Selected '{opt_title}' in AXPopUpButton [{element_index}] \
                         \"{element_title}\" via AX child AXPress; popup AXValue now \
                         \"{settled}\"."
                    )))
                } else {
                    // Unverified child press: fall through to open-then-pick.
                    None
                }
            } else {
                Some(Err(anyhow::anyhow!(
                    "AXPress on child option failed with error {err}"
                )))
            }
        } else {
            // No match among the closed popup's exposed children. Chromium
            // exposes only the currently selected option while closed, so
            // fall through to open-then-pick before reporting the observed
            // children as the full option set.
            None
        };

        // Release children (flatten transfers a retain for each returned ref).
        for &child in &children {
            unsafe { CFRelease(child as _); }
        }

        if let Some(result) = result {
            return result;
        }
        if let Some(message) = typeahead_select_popup_option(element, element_index, pid, value, element_title) {
            return Ok(message);
        }
        if let Some(picked) = open_and_pick_popup_option(element, element_index, pid, value, element_title) {
            return picked;
        }
        let avail = available.iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(", ");
        anyhow::bail!(
            "No AX child matching '{value}' in AXPopUpButton [{element_index}] \
             \"{element_title}\". Available: [{avail}]"
        )
    }

    // ── Strategy 1b: keyboard type-ahead, then open-then-pick ──────────────
    if let Some(message) = typeahead_select_popup_option(element, element_index, pid, value, element_title) {
        return Ok(message);
    }

    // ── Strategy 1c: open-then-pick (Chromium-family popup buttons) ─────────
    // Chromium <select> elements expose no AX children while closed. Press
    // the popup to materialize its menu, re-read the flattened candidates,
    // press the matching option, and verify the popup's observed AXValue.
    // All raw observation; the menu is cancelled on failure so the page is
    // left untouched. Returns None only when the popup never materialized
    // children, so the Safari JS fallback below still applies.
    if let Some(picked) = open_and_pick_popup_option(element, element_index, pid, value, element_title) {
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

/// Keyboard type-ahead selection on a CLOSED popup/select, verified by strict
/// AXValue read-back. This is the interaction a keyboard user performs and it
/// avoids Chromium's AX menu entirely: options pressed in a materializing
/// menu highlight without committing, and even a committed AX pick can leave
/// an orphaned popup-menu window that swallows every subsequent synthesized
/// keystroke (both observed live against an instrumented Chromium fixture).
/// Type-ahead on the closed control commits immediately, opens no menu
/// window, and leaves keyboard routing intact. Returns the success message
/// when the observed AXValue becomes the requested option; None otherwise so
/// the caller can continue its ladder.
fn typeahead_select_popup_option(
    element: AXUIElementRef,
    element_index: usize,
    pid: i32,
    value: &str,
    element_title: &str,
) -> Option<String> {
    let value_lower = value.to_lowercase();
    dismiss_open_popup_menu_window(pid);
    // A type-ahead needs the closed control focused; confirm the focus write
    // observably landed when the element reports AXFocused, bounded so an
    // element without the attribute still proceeds to the strict read-back.
    let _ = crate::input::ax_actions::focus_element(element as usize);
    let focus_deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(500);
    loop {
        if matches!(unsafe { copy_bool_attr(element, "AXFocused") }, Some(true)) {
            break;
        }
        if std::time::Instant::now() >= focus_deadline {
            break;
        }
        let _ = crate::input::ax_actions::focus_element(element as usize);
        std::thread::sleep(std::time::Duration::from_millis(15));
    }

    // Two bounded verified attempts; the read-back decides, never the send.
    for _ in 0..2 {
        if crate::input::keyboard::type_text_with_delay(pid, value, 10).is_err() {
            return None;
        }
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(700);
        loop {
            let observed = unsafe { copy_string_attr(element, "AXValue") }
                .unwrap_or_default();
            if observed.to_lowercase() == value_lower {
                return Some(format!(
                    "✅ Selected '{value}' in AXPopUpButton [{element_index}]                      \"{element_title}\" via keyboard type-ahead; popup AXValue now                      \"{observed}\"."
                ));
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    None
}

/// Chromium routes every synthesized keystroke into an open popup-menu
/// window (layer kCGPopUpMenuWindowLevel), including orphaned menus left by
/// AX menu interactions — observed live as typing silently swallowed for
/// seconds. Before any keystroke-driven step, dismiss such a menu with
/// Escape (consumed by exactly that menu) and confirm the window is gone.
fn dismiss_open_popup_menu_window(pid: i32) {
    if !crate::windows::pid_has_onscreen_popup_window(pid) {
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(600);
    loop {
        let _ = crate::input::keyboard::press_key(pid, "escape", &[]);
        std::thread::sleep(std::time::Duration::from_millis(40));
        if !crate::windows::pid_has_onscreen_popup_window(pid)
            || std::time::Instant::now() >= deadline
        {
            break;
        }
    }
}

/// Explicitly dismiss a popup's open option menu, if any. AXCancel is
/// advertised on the popup's AXMenu container child (not on the popup button
/// itself); performing it on an already-closed menu is a no-op. Bounded
/// observation only — the caller re-reads state to decide the outcome.
fn close_popup_menu(element: AXUIElementRef, pid: i32) {
    if !crate::windows::pid_has_onscreen_popup_window(pid) {
        return;
    }
    let children = unsafe { copy_children(element) };
    for &child in &children {
        let role = unsafe { copy_string_attr(child, "AXRole") }.unwrap_or_default();
        if role == "AXMenu" || role == "AXGroup" || role == "AXList" {
            let _ = unsafe { perform_action(child, "AXCancel") };
        }
        unsafe { CFRelease(child as _); }
    }
    let _ = unsafe { perform_action(element, "AXCancel") };
    dismiss_open_popup_menu_window(pid);
}

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
    pid: i32,
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

    // Poll for the opened menu instead of a fixed sleep, breaking early only
    // when a candidate matching the requested option is observed on two
    // consecutive polls — a closed Chromium popup keeps exposing its
    // currently selected option, so a bare non-empty read is NOT evidence
    // the menu materialized, and a ref observed on a single poll can still
    // be mid-materialization (pressing it is swallowed silently). When no
    // stable match appears, the 250ms ceiling matches the previous fixed
    // wait and the final read decides (listing whatever options were
    // observed).
    let value_lower = value.to_lowercase();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    let mut match_streak = 0u32;
    let opened = loop {
        std::thread::sleep(std::time::Duration::from_millis(20));
        let candidates = flatten_popup_option_candidates(unsafe { copy_children(element) }, 0);
        let has_match = candidates
            .iter()
            .any(|&c| popup_option_text(c).to_lowercase() == value_lower);
        match_streak = if has_match { match_streak + 1 } else { 0 };
        if match_streak >= 2 || std::time::Instant::now() >= deadline {
            break candidates;
        }
        for &candidate in &candidates {
            unsafe { CFRelease(candidate as _); }
        }
    };
    if opened.is_empty() {
        close_popup_menu(element, pid);
        return None;
    }

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
            close_popup_menu(element, pid);
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
                close_popup_menu(element, pid);
                Err(anyhow::anyhow!(
                    "AXPress on opened option '{value}' failed with error {press_err}"
                ))
            } else {
                // The popup's AXValue updates for BOTH a committed activation
                // and an uncommitted menu-highlight preview, and Chromium can
                // leave the menu window open even after a committed pick. A
                // bare AXValue match is therefore not commit evidence: a
                // later focus change closes an uncommitted menu and reverts
                // the preview. Verify deterministically instead: wait for the
                // AXValue match, explicitly close the menu, and require the
                // observed AXValue to survive the close.
                let verify_deadline =
                    std::time::Instant::now() + std::time::Duration::from_millis(600);
                let matched_value = loop {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    let observed = unsafe { copy_string_attr(element, "AXValue") }
                        .unwrap_or_default();
                    if observed.to_lowercase() == value_lower
                        || std::time::Instant::now() >= verify_deadline
                    {
                        break observed;
                    }
                };
                if matched_value.to_lowercase() != value_lower {
                    close_popup_menu(element, pid);
                    Err(anyhow::anyhow!(
                        "❌ Pressed option '{value}' in AXPopUpButton [{element_index}] \
                         \"{element_title}\" but observed AXValue is \"{matched_value}\". \
                         Reread app state and verify the visible selection."
                    ))
                } else {
                    // Close any still-open menu window; a committed selection
                    // survives the close, an uncommitted preview reverts.
                    close_popup_menu(element, pid);
                    let settle_deadline =
                        std::time::Instant::now() + std::time::Duration::from_millis(400);
                    let final_value = loop {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        let menu_open = crate::windows::pid_has_onscreen_popup_window(pid);
                        let observed = unsafe { copy_string_attr(element, "AXValue") }
                            .unwrap_or_default();
                        if !menu_open || std::time::Instant::now() >= settle_deadline {
                            break observed;
                        }
                    };
                    if final_value.to_lowercase() == value_lower {
                        Ok(format!(
                            "✅ Selected '{value}' in AXPopUpButton [{element_index}] \
                             \"{element_title}\" via open-then-pick; popup AXValue now \
                             \"{final_value}\" after the menu closed."
                        ))
                    } else {
                        Err(anyhow::anyhow!(
                            "❌ Pressed option '{value}' in AXPopUpButton [{element_index}] \
                             \"{element_title}\" but the selection reverted to \
                             \"{final_value}\" when the menu closed — the pick highlighted \
                             without committing. Reread app state and retry."
                        ))
                    }
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
