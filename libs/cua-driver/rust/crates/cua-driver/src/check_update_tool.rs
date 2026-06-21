//! `check_for_update` MCP tool — programmatic mirror of
//! `cua-driver check-update`.
//!
//! Lives in the `cua-driver` binary crate (not in `cua-driver-core` or the
//! per-platform `tools/` modules) so every platform's tool table can expose
//! the same update-state shape. Private fork policy disables remote update
//! checks, so the tool reports the disabled state without network access.
//!
//! Registered by [`register_into`] from `main.rs` immediately after the
//! per-platform `register_tools()` call, so the result is identical to
//! the per-platform tools registering it themselves.

use async_trait::async_trait;
use serde_json::Value;

use cua_driver_core::{
    protocol::ToolResult,
    tool::{Tool, ToolDef, ToolRegistry},
};

pub struct CheckForUpdateTool;

static DEF: std::sync::OnceLock<ToolDef> = std::sync::OnceLock::new();

fn def() -> &'static ToolDef {
    DEF.get_or_init(|| ToolDef {
        name: "check_for_update".into(),
        description:
            "Return the private fork's update-check state. Remote update checks are \
             disabled, so this reports the current version and disabled-state error \
             without network access. Mirror of `cua-driver check-update --json`."
                .into(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
        read_only: true,
        destructive: false,
        idempotent: true,
        open_world: false,
    })
}

#[async_trait]
impl Tool for CheckForUpdateTool {
    fn def(&self) -> &ToolDef {
        def()
    }

    async fn invoke(&self, _args: Value) -> ToolResult {
        let state = tokio::task::spawn_blocking(|| {
            crate::version_check::check_update_state(false)
        })
        .await
        .expect("version_check::check_update_state never panics");

        let summary = if let Some(err) = &state.error {
            format!("Update check failed: {err}")
        } else if state.update_available {
            let latest = state.latest_version.as_deref().unwrap_or("?");
            format!(
                "Update available: cua-driver {latest} (you have {}).",
                state.current_version
            )
        } else {
            format!("Up to date (cua-driver {}).", state.current_version)
        };

        let structured = serde_json::to_value(&state)
            .unwrap_or_else(|_| serde_json::json!({}));

        ToolResult::text(summary).with_structured(structured)
    }
}

/// Register the `check_for_update` tool into a freshly-built platform
/// registry. Call site lives in `main.rs`; this keeps the per-platform
/// `register_tools()` signature untouched.
pub fn register_into(registry: &mut ToolRegistry) {
    registry.register(Box::new(CheckForUpdateTool));
}
