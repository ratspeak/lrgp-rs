//! LRGP app interface.

use std::collections::HashMap;

use serde_json::Value as JsonValue;

use crate::envelope::Envelope;
use crate::errors::LrgpError;
use crate::session::Session;

#[derive(Debug, Clone)]
pub struct IncomingResult {
    pub session: Option<HashMap<String, JsonValue>>,
    pub emit: Option<HashMap<String, JsonValue>>,
    pub error: Option<HashMap<String, JsonValue>>,
}

#[derive(Debug, Clone)]
pub struct OutgoingResult {
    pub payload: HashMap<String, rmpv::Value>,
    /// Plain-text fallback rendered for non-LRGP clients.
    pub fallback_text: String,
}

/// Explicit result of router-owned inbound replay filtering.
#[derive(Debug, Clone)]
pub enum IncomingDispatch {
    Applied(IncomingResult),
    /// A validated, authenticated protocol error reported by the remote peer.
    /// This is observational and must never be answered with another `error`.
    RemoteError(RemoteProtocolError),
    Replay,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteProtocolError {
    pub app_id: String,
    pub session_id: String,
    pub code: String,
    pub message: String,
    pub reference: String,
}

/// Fully validated outbound action, ready for the LXMF integration.
#[derive(Debug, Clone)]
pub struct PreparedOutgoing {
    pub envelope: Envelope,
    /// Canonical session ID. This matters when a challenge requested automatic
    /// ID generation by passing an empty session ID.
    pub session_id: String,
    pub fallback_text: String,
    pub delivery_method: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AppManifest {
    pub app_id: String,
    pub version: u32,
    pub display_name: String,
    pub icon: String,
    pub session_type: String,
    pub max_players: u32,
    pub validation: String,
    pub actions: Vec<String>,
    pub preferred_delivery: HashMap<String, String>,
    pub ttl: HashMap<String, f64>,
}

pub trait GameApp: Send + Sync {
    fn app_id(&self) -> &str;
    fn version(&self) -> u32;
    fn manifest(&self) -> AppManifest;

    fn handle_incoming(
        &self,
        session_id: &str,
        command: &str,
        payload: &HashMap<String, rmpv::Value>,
        sender_hash: &str,
        identity_id: &str,
    ) -> IncomingResult;

    fn handle_outgoing(
        &self,
        session_id: &str,
        command: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
    ) -> OutgoingResult;

    fn validate_action(
        &self,
        session_id: &str,
        command: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
    ) -> (bool, Option<String>);

    /// Validate a local user intent before any outgoing handler mutates state.
    /// Incoming move validation often expects enriched wire fields, while the
    /// outgoing UI supplies only the user's concise intent, so the two paths
    /// are intentionally distinct.
    fn validate_outgoing_action(
        &self,
        _session_id: &str,
        _command: &str,
        _payload: &HashMap<String, rmpv::Value>,
        _identity_id: &str,
    ) -> (bool, Option<String>) {
        (true, None)
    }

    fn get_session_state(&self, session_id: &str, identity_id: &str) -> HashMap<String, JsonValue>;

    fn render_fallback(&self, command: &str, payload: &HashMap<String, rmpv::Value>) -> String;

    fn get_delivery_method(&self, command: &str) -> String {
        let _ = command;
        "opportunistic".to_string()
    }

    /// Return a complete session record, applying app TTL policy before it is
    /// returned. Implementations accepting challenges MUST implement this and
    /// the list/binding/authorization methods below. An external persistence
    /// integration must durably save any returned `expired` transition.
    fn get_session_record(&self, _session_id: &str, _identity_id: &str) -> Option<Session> {
        None
    }

    /// Restore or replace one persisted session in the app's live state.
    fn upsert_session(&self, _session: Session) -> Result<(), LrgpError> {
        Err(LrgpError::Validation {
            code: "unsupported_operation".into(),
            message: "app does not support session restore".into(),
        })
    }

    /// Delete one session from the app's live state.
    fn remove_session(&self, _session_id: &str, _identity_id: &str) -> bool {
        false
    }

    /// List live session records, optionally restricted to one local identity.
    /// Implementations accepting challenges MUST return all of their records
    /// here so router-wide admission limits cannot be bypassed.
    fn list_session_records(&self, _identity_id: Option<&str>) -> Vec<Session> {
        Vec::new()
    }

    /// Bind the expected remote peer to a newly-created outgoing challenge.
    fn bind_session_peer(
        &self,
        _session_id: &str,
        _identity_id: &str,
        _peer_hash: &str,
    ) -> Result<(), LrgpError> {
        Err(LrgpError::Validation {
            code: "unsupported_operation".into(),
            message: "app does not support participant binding".into(),
        })
    }

    /// Authorize the transport-authenticated sender for an inbound action.
    /// Challenge handlers establish the binding; every other command must be
    /// from the session's bound remote participant.
    fn authorize_incoming(
        &self,
        session_id: &str,
        command: &str,
        sender_hash: &str,
        identity_id: &str,
    ) -> Result<(), LrgpError> {
        let _ = (command, sender_hash, identity_id);
        Err(LrgpError::Validation {
            code: "unsupported_operation".into(),
            message: format!(
                "app does not implement participant authorization for session {session_id}"
            ),
        })
    }

    /// Snapshot pre-mutation state for transactional rollback. `None` for new
    /// sessions or apps that don't support rollback. Call before `handle_outgoing`.
    fn snapshot_session(&self, _session_id: &str, _identity_id: &str) -> Option<Session> {
        None
    }

    /// Reverse a `handle_outgoing` mutation. `Some(snap)` restores prior state;
    /// `None` deletes a freshly created session.
    fn rollback_session(&self, _session_id: &str, _identity_id: &str, _snapshot: Option<Session>) {}
}
