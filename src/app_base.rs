//! LRGP app interface.

use std::collections::HashMap;

use serde_json::Value as JsonValue;

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
        sender_hash: &str,
    ) -> (bool, Option<String>);

    fn get_session_state(&self, session_id: &str, identity_id: &str) -> HashMap<String, JsonValue>;

    fn render_fallback(&self, command: &str, payload: &HashMap<String, rmpv::Value>) -> String;

    fn get_delivery_method(&self, command: &str) -> String {
        let _ = command;
        "opportunistic".to_string()
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
