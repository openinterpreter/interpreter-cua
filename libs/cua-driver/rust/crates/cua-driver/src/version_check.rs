//! Automatic update checks are disabled in this private fork.

#![allow(dead_code)]

use std::time::{SystemTime, UNIX_EPOCH};

pub const RELEASE_TAG_PREFIX: &str = "cua-driver-rs-v";

#[derive(serde::Serialize, Debug, Clone)]
pub struct UpdateState {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub source: &'static str,
    pub checked_at: String,
    pub cache_hit: bool,
    pub install_command: Option<String>,
    pub release_notes_url: Option<String>,
    pub error: Option<String>,
}

pub fn maybe_announce_update() {}

pub fn check_update_state(_no_cache: bool) -> UpdateState {
    UpdateState {
        current_version: env!("CARGO_PKG_VERSION").to_owned(),
        latest_version: None,
        update_available: false,
        source: "disabled",
        checked_at: checked_at(),
        cache_hit: false,
        install_command: None,
        release_notes_url: None,
        error: Some("update checks are disabled in this fork".to_owned()),
    }
}

pub fn dismiss_version(_version: &str) {}

pub fn is_newer(latest: &str, current: &str) -> bool {
    let Some(latest_parts) = version_parts(latest) else {
        return false;
    };
    let Some(current_parts) = version_parts(current) else {
        return false;
    };

    for (latest_part, current_part) in latest_parts.iter().zip(current_parts.iter()) {
        if latest_part != current_part {
            return latest_part > current_part;
        }
    }
    latest_parts.len() > current_parts.len()
}

pub fn fetch_latest_version() -> Result<String, String> {
    Err("update checks are disabled in this fork".to_owned())
}

fn version_parts(version: &str) -> Option<Vec<u64>> {
    let core = version.split_once('-').map_or(version, |(core, _)| core);
    let parts: Option<Vec<u64>> = core.split('.').map(|part| part.parse().ok()).collect();
    parts.filter(|value| !value.is_empty())
}

fn checked_at() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("unix:{seconds}")
}
