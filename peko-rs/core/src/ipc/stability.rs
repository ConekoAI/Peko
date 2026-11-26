//! Which wire types will not change (ADR-121 D6).
//!
//! Every `RequestPacket` and `ResponsePacket` type is listed here by its
//! wire name (`"type"`), marked `Stable` or `Internal`. For a `Stable`
//! type ADR-102 applies as written: a breaking change goes out in a minor
//! version. An `Internal` type may change in a patch. Every door accepts
//! both; the mark is a promise about change, not a permission.
//!
//! The first `Stable` set is what the command line sends today, plus the
//! channel reads, watches and approvals a service relaying conversations
//! needs (`channel_events_watch`, `channel_history_older`,
//! `channel_mark_read`, `channel_read_state`, `channel_peko_approvals`,
//! `channel_peko_approval_decide`), and the answers to them. Peko
//! Desktop's own settings (models, credentials, bindings, plan sign-in)
//! stay `Internal`.
//!
//! A test fails when a type in `packet.rs` is missing here, or a name
//! here is no longer in `packet.rs`: a new request is marked when it is
//! added.
//!
//! A new optional field on a type keeps its mark: older senders omit it
//! and older readers ignore it (ADR-126's `reply_to` on `principal_send`,
//! `principal_send_stream` and `channel_post`, `react` on `channel_post`,
//! and `reactions` on `principal_log` rows and `channel_peek_result`;
//! ADR-127's `seed_from` on `principal_create`).

/// What a wire type promises about change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stability {
    /// Breaks only in a minor version (ADR-102).
    Stable,
    /// May change in a patch.
    Internal,
}

use Stability::{Internal, Stable};

impl Stability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Internal => "internal",
        }
    }
}

/// `RequestPacket` types, by wire name.
pub const REQUESTS: &[(&str, Stability)] = &[
    ("approval_decide", Stable),
    ("approval_list", Stable),
    ("audit_query", Stable),
    ("auth_api_key_create", Stable),
    ("auth_api_key_list", Stable),
    ("auth_api_key_revoke", Stable),
    ("auth_status", Stable),
    ("binding_delete", Internal),
    ("binding_get", Internal),
    ("binding_list", Internal),
    ("binding_set", Internal),
    ("browser", Stable),
    ("channel_block_person", Stable),
    ("channel_blocked", Stable),
    ("channel_board", Stable),
    ("channel_create", Stable),
    ("channel_dm_request_answer", Stable),
    ("channel_dm_requests", Stable),
    ("channel_events_watch", Stable),
    ("channel_group_join", Stable),
    ("channel_group_link", Stable),
    ("channel_group_register", Stable),
    ("channel_history_older", Stable),
    ("channel_info", Stable),
    ("channel_invitation_answer", Stable),
    ("channel_invitations", Stable),
    ("channel_invite", Stable),
    ("channel_leave", Stable),
    ("channel_list", Stable),
    ("channel_mark_read", Stable),
    ("channel_members", Stable),
    ("channel_open_person_dm", Stable),
    ("channel_open_remote_dm", Stable),
    ("channel_peek", Stable),
    ("channel_peko_approval_decide", Stable),
    ("channel_peko_approvals", Stable),
    ("channel_peko_browser", Internal),
    ("channel_peko_dms", Internal),
    ("channel_peko_models", Internal),
    ("channel_person_dms", Internal),
    ("channel_person_groups", Internal),
    ("channel_pin_to_shared", Stable),
    ("channel_post", Stable),
    ("channel_read_state", Stable),
    ("channel_set_wake", Internal),
    ("channel_undelivered", Internal),
    ("channel_wakes", Stable),
    ("chatgpt_sign_in_cancel", Internal),
    ("chatgpt_sign_in_start", Internal),
    ("chatgpt_sign_in_status", Internal),
    ("computer_resume", Internal),
    ("computer_setup", Internal),
    ("computer_state", Internal),
    ("computer_stop", Internal),
    ("credential_delete", Internal),
    ("credential_get", Internal),
    ("credential_get_material", Internal),
    ("credential_list", Internal),
    ("credential_set", Internal),
    ("device_add", Stable),
    ("device_join", Stable),
    ("device_list", Stable),
    ("device_remove", Stable),
    ("execute_tool", Stable),
    ("identity_code", Stable),
    ("instance_set_exposure", Internal),
    ("instance_set_status", Internal),
    ("model_add", Internal),
    ("model_discover", Internal),
    ("model_list", Internal),
    ("model_reload", Stable),
    ("model_remove", Internal),
    ("model_templates", Internal),
    ("model_test", Stable),
    ("model_update", Internal),
    ("network_link_retry", Internal),
    ("network_settings_set", Stable),
    ("network_status", Stable),
    ("person_name_set", Stable),
    ("ping", Stable),
    ("principal_access_get", Internal),
    ("principal_access_set", Internal),
    ("principal_create", Stable),
    ("principal_export", Stable),
    ("principal_get", Stable),
    ("principal_grant_permission", Stable),
    ("principal_import", Stable),
    ("principal_import_preview", Stable),
    ("principal_list", Stable),
    ("principal_log", Stable),
    ("principal_log_watch", Stable),
    ("principal_mint_invite", Stable),
    ("principal_permissions", Stable),
    ("principal_register", Stable),
    ("principal_reload", Internal),
    ("principal_remove", Stable),
    ("principal_revoke_invite", Stable),
    ("principal_revoke_permission", Stable),
    ("principal_send", Stable),
    ("principal_send_stream", Stable),
    ("principal_sent_watch", Stable),
    ("principal_set_status", Stable),
    ("principal_setup_run", Stable),
    ("principal_setup_status", Stable),
    ("principal_stop", Stable),
    ("principal_transfer", Stable),
    ("principal_update", Stable),
    ("quota_get", Stable),
    ("quota_reset", Stable),
    ("quota_set", Stable),
    ("runtime_id", Stable),
    ("runtime_info", Stable),
    ("runtime_register", Stable),
    ("runtime_unregister", Internal),
    ("secret_decline", Stable),
    ("secret_delete", Stable),
    ("secret_list", Stable),
    ("secret_provide", Stable),
    ("security_decide", Stable),
    ("seed_publish", Stable),
    ("shutdown", Internal),
    ("status", Stable),
    ("system_clean", Stable),
    ("system_doctor", Stable),
    ("system_status", Stable),
    ("tunnel_status", Stable),
    ("tunnel_stop", Stable),
    ("turn_changes", Stable),
    ("turn_undo", Stable),
];

/// `ResponsePacket` types, by wire name.
pub const RESPONSES: &[(&str, Stability)] = &[
    ("approval_decided", Stable),
    ("approval_list", Stable),
    ("audit_events", Stable),
    ("auth_api_key_created", Stable),
    ("auth_api_key_list", Stable),
    ("auth_api_key_revoked", Stable),
    ("auth_status", Stable),
    ("binding_deleted", Internal),
    ("binding_set_done", Internal),
    ("bindings_listed", Internal),
    ("browser_result", Stable),
    ("channel_blocked_people", Stable),
    ("channel_board_result", Stable),
    ("channel_created", Stable),
    ("channel_dm_requests_result", Stable),
    ("channel_event_received", Stable),
    ("channel_group_joined", Stable),
    ("channel_group_link_result", Stable),
    ("channel_group_registered", Stable),
    ("channel_history_page", Stable),
    ("channel_info_result", Stable),
    ("channel_invitations_result", Stable),
    ("channel_invited", Stable),
    ("channel_left", Stable),
    ("channel_list_result", Stable),
    ("channel_member_left", Stable),
    ("channel_members_result", Stable),
    ("channel_peek_result", Stable),
    ("channel_peko_approvals_result", Stable),
    ("channel_peko_dms_result", Internal),
    ("channel_peko_models_result", Internal),
    ("channel_person_dm_opened", Stable),
    ("channel_person_dms_result", Internal),
    ("channel_person_groups_result", Internal),
    ("channel_pinned_to_shared", Stable),
    ("channel_posted", Stable),
    ("channel_read_marked", Stable),
    ("channel_read_state_result", Stable),
    ("channel_remote_dm_opened", Stable),
    ("channel_undelivered_result", Internal),
    ("channel_wake_set", Internal),
    ("channel_wakes_result", Stable),
    ("chatgpt_sign_in_started", Internal),
    ("chatgpt_sign_in_state", Internal),
    ("computer_result", Internal),
    ("credential_deleted", Internal),
    ("credential_got", Internal),
    ("credential_material", Internal),
    ("credential_set_done", Internal),
    ("credentials_listed", Internal),
    ("device_code_shown", Stable),
    ("device_list_result", Stable),
    ("done", Stable),
    ("error", Stable),
    ("heartbeat", Stable),
    ("identity_code_result", Stable),
    ("model_added", Internal),
    ("model_list", Internal),
    ("model_reloaded", Stable),
    ("model_removed", Internal),
    ("model_templates", Internal),
    ("model_tested", Stable),
    ("model_updated", Internal),
    ("models_discovered", Internal),
    ("network_status_result", Stable),
    ("pong", Stable),
    ("principal_access", Internal),
    ("principal_created", Stable),
    ("principal_exported", Stable),
    ("principal_get", Stable),
    ("principal_import_previewed", Stable),
    ("principal_imported", Stable),
    ("principal_invite_minted", Stable),
    ("principal_invite_revoked", Stable),
    ("principal_list", Stable),
    ("principal_loaded", Internal),
    ("principal_log", Stable),
    ("principal_log_appended", Stable),
    ("principal_owner_transferred", Stable),
    ("principal_permission_granted", Stable),
    ("principal_permission_revoked", Stable),
    ("principal_permissions", Stable),
    ("principal_registration_updated", Stable),
    ("principal_removed", Stable),
    ("principal_sent", Stable),
    ("principal_sent_appended", Stable),
    ("principal_sent_chunk", Stable),
    ("principal_sent_done", Stable),
    ("principal_sent_iteration", Stable),
    ("principal_sent_successor", Stable),
    ("principal_sent_thinking", Stable),
    ("principal_sent_tool_end", Stable),
    ("principal_sent_tool_start", Stable),
    ("principal_setup_status", Stable),
    ("principal_status_updated", Stable),
    ("principal_transfer_offered", Stable),
    ("principal_updated", Stable),
    ("quota_status", Stable),
    ("run_summary", Stable),
    ("runtime_id", Stable),
    ("runtime_info", Stable),
    ("runtime_registered", Stable),
    ("secret_done", Stable),
    ("secret_list_result", Stable),
    ("seed_published", Stable),
    ("shutting_down", Internal),
    ("status", Stable),
    ("system_cleaned", Stable),
    ("system_doctor", Stable),
    ("system_status", Stable),
    ("text", Stable),
    ("tool_executed", Stable),
    ("tunnel_status", Stable),
    ("turn_changes_result", Stable),
    ("turn_undone", Stable),
];

/// The mark of a request type.
#[must_use]
pub fn of_request(wire: &str) -> Option<Stability> {
    lookup(REQUESTS, wire)
}

/// The mark of a response type.
#[must_use]
pub fn of_response(wire: &str) -> Option<Stability> {
    lookup(RESPONSES, wire)
}

fn lookup(table: &[(&str, Stability)], wire: &str) -> Option<Stability> {
    table
        .binary_search_by(|(name, _)| (*name).cmp(wire))
        .ok()
        .map(|i| table[i].1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The wire names of `enum_name`'s variants, read from `packet.rs`.
    /// Each variant must carry `#[serde(rename = "...")]`, as all do.
    fn wire_names(enum_name: &str) -> BTreeSet<String> {
        let source = include_str!("packet.rs");
        let start = source
            .find(&format!("pub enum {enum_name} {{"))
            .unwrap_or_else(|| panic!("{enum_name} not in packet.rs"));
        let mut depth = 0usize;
        let mut end = start;
        for (i, c) in source[start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let mut names = BTreeSet::new();
        let mut pending: Option<String> = None;
        for line in source[start..end].lines().skip(1) {
            if let Some(rest) = line.strip_prefix("    #[serde(rename = \"") {
                pending = rest.split('"').next().map(str::to_string);
                continue;
            }
            let variant = line
                .strip_prefix("    ")
                .filter(|l| l.starts_with(|c: char| c.is_ascii_uppercase()));
            if let Some(variant) = variant {
                let name = pending.take().unwrap_or_else(|| {
                    panic!(
                        "{enum_name}::{variant} has no #[serde(rename)]: name it and mark it here"
                    )
                });
                assert!(names.insert(name.clone()), "{name} appears twice");
            }
        }
        names
    }

    fn check(enum_name: &str, table: &[(&str, Stability)]) {
        let listed: Vec<&str> = table.iter().map(|(name, _)| *name).collect();
        let mut sorted = listed.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            listed, sorted,
            "{enum_name}: keep the table sorted, each name once"
        );
        let listed: BTreeSet<String> = listed.into_iter().map(str::to_string).collect();
        let wire = wire_names(enum_name);
        let unmarked: Vec<_> = wire.difference(&listed).collect();
        assert!(
            unmarked.is_empty(),
            "{enum_name} types not marked stable or internal in ipc/stability.rs: {unmarked:?}"
        );
        let gone: Vec<_> = listed.difference(&wire).collect();
        assert!(
            gone.is_empty(),
            "marked but no longer in {enum_name}: {gone:?}"
        );
    }

    #[test]
    fn every_request_type_is_marked() {
        check("RequestPacket", REQUESTS);
    }

    #[test]
    fn every_response_type_is_marked() {
        check("ResponsePacket", RESPONSES);
    }

    #[test]
    fn marks_are_found_by_name() {
        assert_eq!(of_request("principal_send"), Some(Stability::Stable));
        assert_eq!(of_request("credential_set"), Some(Stability::Internal));
        assert_eq!(of_response("error"), Some(Stability::Stable));
        assert_eq!(
            of_response("credential_material"),
            Some(Stability::Internal)
        );
        assert_eq!(of_request("no_such_request"), None);
    }

    /// Every stable request answers with stable types: spot-check the
    /// ones a service relaying conversations uses.
    #[test]
    fn a_relaying_service_stays_on_stable_types() {
        for request in [
            "ping",
            "principal_send",
            "principal_send_stream",
            "principal_sent_watch",
            "principal_log",
            "approval_list",
            "approval_decide",
            "channel_events_watch",
        ] {
            assert_eq!(of_request(request), Some(Stability::Stable), "{request}");
        }
        for response in [
            "pong",
            "principal_sent",
            "principal_sent_chunk",
            "principal_sent_done",
            "principal_sent_appended",
            "principal_log",
            "approval_list",
            "approval_decided",
            "channel_event_received",
            "heartbeat",
            "done",
            "error",
        ] {
            assert_eq!(of_response(response), Some(Stability::Stable), "{response}");
        }
    }
}
