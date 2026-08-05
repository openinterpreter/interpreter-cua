//! Distribution policy for the private Interpreter CUA fork.
//!
//! Keep private-product policy here instead of replacing upstream subsystems.
//! This makes upstream merges small and auditable while ensuring that a new
//! upstream call site cannot accidentally opt this distribution into a remote
//! service merely because the user has not configured an opt-out.

pub const REMOTE_TELEMETRY_ENABLED: bool = false;
pub const UPSTREAM_UPDATE_CHECKS_ENABLED: bool = false;

pub const REMOTE_TELEMETRY_DISABLED_REASON: &str =
    "remote telemetry is disabled in the Interpreter CUA private fork";
pub const UPSTREAM_UPDATE_CHECKS_DISABLED_REASON: &str =
    "upstream update checks are disabled in the Interpreter CUA private fork";

/// Unit tests for upstream's telemetry implementation must still exercise its
/// real state machine. Production and integration binaries remain fail-closed.
pub fn remote_telemetry_enabled() -> bool {
    #[cfg(test)]
    {
        true
    }
    #[cfg(not(test))]
    {
        REMOTE_TELEMETRY_ENABLED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_distribution_is_fail_closed() {
        assert!(!REMOTE_TELEMETRY_ENABLED);
        assert!(!UPSTREAM_UPDATE_CHECKS_ENABLED);
    }
}
