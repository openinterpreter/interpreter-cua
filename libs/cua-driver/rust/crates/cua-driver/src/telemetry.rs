//! Telemetry is disabled in this private fork.

#![allow(dead_code)]

pub mod event {
    pub const INSTALL: &str = "cua_driver_install";
    pub const MCP: &str = "cua_driver_mcp";
    pub const SERVE: &str = "cua_driver_serve";
    pub const STOP: &str = "cua_driver_stop";
    pub const STATUS: &str = "cua_driver_status";
    pub const CALL: &str = "cua_driver_call";
    pub const LIST_TOOLS: &str = "cua_driver_list_tools";
    pub const DESCRIBE: &str = "cua_driver_describe";
    pub const RECORDING: &str = "cua_driver_recording";
    pub const CONFIG: &str = "cua_driver_config";
    pub const GUI_LAUNCH: &str = "cua_driver_gui_launch";
    pub const API_PREFIX: &str = "cua_driver_api_";
}

pub fn is_enabled() -> bool {
    false
}

pub fn capture(_event_name: &str, _properties: Option<serde_json::Value>) {}

pub fn capture_install() {}
