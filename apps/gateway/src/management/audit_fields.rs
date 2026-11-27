//! Audit metadata fields allowed for one kind of action only, beyond the
//! generic scalar allowlist (`resources::audit`): each module checks its own
//! fields, for its own actions.

/// Whether `key` with `value` may be recorded in the metadata of `action`.
pub(super) fn allowed(action: &str, key: &str, value: &serde_json::Value) -> bool {
    super::guardrails::audit_field(action, key, value)
        || super::drain::audit_field(action, key, value)
        || super::deployment_video::audit_field(action, key, value)
}
