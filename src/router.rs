/// LRGP game router — registry for game implementations and dispatch of
/// incoming/outgoing game messages.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::app_base::{GameApp, AppManifest, IncomingResult, OutgoingResult};
use crate::constants::*;
use crate::envelope::{self, Envelope};
use crate::errors::LrgpError;
use crate::session::Session;

/// Thread-safe registry of LRGP game implementations.
pub struct LrgpRouter {
    apps: Mutex<HashMap<String, Arc<dyn GameApp>>>,
}

impl LrgpRouter {
    pub fn new() -> Self {
        Self {
            apps: Mutex::new(HashMap::new()),
        }
    }

    /// Register a game implementation.
    pub fn register(&self, app: Box<dyn GameApp>) {
        let id = app.app_id().to_string();
        let arc: Arc<dyn GameApp> = Arc::from(app);
        self.apps.lock().unwrap().insert(id, arc);
    }

    /// List manifests for all registered games.
    pub fn list_apps(&self) -> Vec<AppManifest> {
        let apps = self.apps.lock().unwrap();
        apps.values().map(|a| a.manifest()).collect()
    }

    /// Execute a callback on a registered game by app_id.
    pub fn with_app<F, R>(&self, app_id: &str, f: F) -> Option<R>
    where
        F: FnOnce(&dyn GameApp) -> R,
    {
        let apps = self.apps.lock().unwrap();
        apps.get(app_id).map(|app| f(app.as_ref()))
    }

    /// Dispatch an incoming LRGP envelope to the appropriate game.
    pub fn dispatch_incoming(
        &self,
        envelope: &Envelope,
        sender_hash: &str,
        identity_id: &str,
    ) -> Result<IncomingResult, LrgpError> {
        let app_ver = envelope
            .get(KEY_APP)
            .and_then(|v| envelope::value_as_str(v))
            .ok_or_else(|| LrgpError::InvalidEnvelope("missing 'a' key".into()))?;

        let (app_id, _version) = envelope::parse_app_version(app_ver)
            .ok_or_else(|| LrgpError::InvalidEnvelope("invalid app.version format".into()))?;

        let command = envelope
            .get(KEY_COMMAND)
            .and_then(|v| envelope::value_as_str(v))
            .ok_or_else(|| LrgpError::InvalidEnvelope("missing 'c' key".into()))?;

        let session_id = envelope
            .get(KEY_SESSION)
            .and_then(|v| envelope::value_as_str(v))
            .ok_or_else(|| LrgpError::InvalidEnvelope("missing 's' key".into()))?;

        let payload: HashMap<String, rmpv::Value> = envelope
            .get(KEY_PAYLOAD)
            .and_then(envelope::map_from_value)
            .unwrap_or_default();

        let apps = self.apps.lock().unwrap();
        let app = apps
            .get(app_id)
            .ok_or_else(|| LrgpError::UnknownApp(app_id.to_string()))?;

        Ok(app.handle_incoming(session_id, command, &payload, sender_hash, identity_id))
    }

    /// Dispatch an outgoing action: build envelope + payload for sending.
    pub fn dispatch_outgoing(
        &self,
        app_id: &str,
        version: u32,
        command: &str,
        session_id: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
    ) -> Result<(Envelope, String), LrgpError> {
        let apps = self.apps.lock().unwrap();
        let app = apps
            .get(app_id)
            .ok_or_else(|| LrgpError::UnknownApp(app_id.to_string()))?;

        let result: OutgoingResult =
            app.handle_outgoing(session_id, command, payload, identity_id);

        let env = envelope::pack_envelope(app_id, version, command, session_id, Some(result.payload), None);
        Ok((env, result.fallback_text))
    }

    /// Snapshot pre-mutation session state so a failed LXMF send after
    /// [`dispatch_outgoing`](Self::dispatch_outgoing) can be reversed via
    /// [`rollback_outgoing`](Self::rollback_outgoing).
    ///
    /// Returns `None` for unknown apps, apps that don't implement rollback
    /// (the [`GameApp::snapshot_session`] default returns `None`), or sessions
    /// that don't yet exist. The recommended transactional pattern is:
    ///
    /// ```ignore
    /// let snap = router.snapshot_before_outgoing(app_id, session_id, identity_id);
    /// let (envelope, fallback) = router.dispatch_outgoing(...)?;
    /// match send_lxmf(envelope, fallback) {
    ///     Ok(_) => { /* committed */ }
    ///     Err(_) => router.rollback_outgoing(app_id, session_id, identity_id, snap)?,
    /// }
    /// ```
    pub fn snapshot_before_outgoing(
        &self,
        app_id: &str,
        session_id: &str,
        identity_id: &str,
    ) -> Option<Session> {
        let apps = self.apps.lock().unwrap();
        let app = match apps.get(app_id) {
            Some(a) => a,
            None => {
                tracing::warn!(
                    app_id,
                    session_id,
                    "snapshot_before_outgoing: unknown app \u{2014} dispatch will be unrollbackable"
                );
                return None;
            }
        };
        let snap = app.snapshot_session(session_id, identity_id);
        if snap.is_none() {
            tracing::debug!(
                app_id,
                session_id,
                "snapshot_before_outgoing: no snapshot (new session, or app does not implement rollback)"
            );
        }
        snap
    }

    /// Reverse a [`dispatch_outgoing`](Self::dispatch_outgoing) mutation
    /// after a failed LXMF send. `Some(session)` restores prior state via
    /// [`GameApp::rollback_session`]; `None` means the dispatch created a
    /// fresh session that should now be deleted (the app decides how).
    /// See [`snapshot_before_outgoing`](Self::snapshot_before_outgoing) for
    /// the full transactional pattern.
    pub fn rollback_outgoing(
        &self,
        app_id: &str,
        session_id: &str,
        identity_id: &str,
        snapshot: Option<Session>,
    ) -> Result<(), LrgpError> {
        let apps = self.apps.lock().unwrap();
        let app = apps
            .get(app_id)
            .ok_or_else(|| LrgpError::UnknownApp(app_id.to_string()))?;
        app.rollback_session(session_id, identity_id, snapshot);
        Ok(())
    }
}

impl Default for LrgpRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_base::*;
    use serde_json::Value as JsonValue;

    /// Minimal mock game for testing the router.
    struct MockGame;
    impl GameApp for MockGame {
        fn app_id(&self) -> &str {
            "mock"
        }
        fn version(&self) -> u32 {
            1
        }
        fn manifest(&self) -> AppManifest {
            AppManifest {
                app_id: "mock".into(),
                version: 1,
                display_name: "Mock Game".into(),
                icon: "mock".into(),
                session_type: SESSION_TURN_BASED.into(),
                max_players: 2,
                validation: VALIDATION_BOTH.into(),
                actions: vec![CMD_CHALLENGE.into(), CMD_MOVE.into()],
                preferred_delivery: HashMap::new(),
                ttl: HashMap::new(),
            }
        }
        fn handle_incoming(
            &self,
            _session_id: &str,
            command: &str,
            _payload: &HashMap<String, rmpv::Value>,
            _sender_hash: &str,
            _identity_id: &str,
        ) -> IncomingResult {
            IncomingResult {
                session: None,
                emit: Some({
                    let mut m = HashMap::new();
                    m.insert("type".into(), JsonValue::String(command.into()));
                    m
                }),
                error: None,
            }
        }
        fn handle_outgoing(
            &self,
            _session_id: &str,
            command: &str,
            _payload: &HashMap<String, rmpv::Value>,
            _identity_id: &str,
        ) -> OutgoingResult {
            OutgoingResult {
                payload: HashMap::new(),
                fallback_text: format!("[LRGP Mock] {command}"),
            }
        }
        fn validate_action(
            &self,
            _session_id: &str,
            _command: &str,
            _payload: &HashMap<String, rmpv::Value>,
            _sender_hash: &str,
        ) -> (bool, Option<String>) {
            (true, None)
        }
        fn get_session_state(
            &self,
            _session_id: &str,
            _identity_id: &str,
        ) -> HashMap<String, JsonValue> {
            HashMap::new()
        }
        fn render_fallback(
            &self,
            command: &str,
            _payload: &HashMap<String, rmpv::Value>,
        ) -> String {
            format!("[LRGP Mock] {command}")
        }
    }

    #[test]
    fn test_register_and_list() {
        let router = LrgpRouter::new();
        router.register(Box::new(MockGame));
        let apps = router.list_apps();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].app_id, "mock");
    }

    #[test]
    fn test_dispatch_incoming() {
        let router = LrgpRouter::new();
        router.register(Box::new(MockGame));

        let env = envelope::pack_envelope("mock", 1, "challenge", "sess1", None, None);
        let result = router.dispatch_incoming(&env, "sender", "local").unwrap();
        assert!(result.error.is_none());
        assert!(result.emit.is_some());
    }

    #[test]
    fn test_dispatch_incoming_unknown_app() {
        let router = LrgpRouter::new();
        let env = envelope::pack_envelope("unknown", 1, "challenge", "sess1", None, None);
        let result = router.dispatch_incoming(&env, "sender", "local");
        assert!(matches!(result, Err(LrgpError::UnknownApp(_))));
    }

    #[test]
    fn test_dispatch_outgoing() {
        let router = LrgpRouter::new();
        router.register(Box::new(MockGame));

        let (env, fallback) = router
            .dispatch_outgoing("mock", 1, "challenge", "sess1", &HashMap::new(), "local")
            .unwrap();
        assert!(env.contains_key(KEY_APP));
        assert_eq!(fallback, "[LRGP Mock] challenge");
    }

    #[test]
    fn test_with_app() {
        let router = LrgpRouter::new();
        router.register(Box::new(MockGame));
        let result = router.with_app("mock", |app| app.manifest().display_name);
        assert_eq!(result, Some("Mock Game".to_string()));
    }

    /// MockGame uses the default GameApp::snapshot_session impl (returns None)
    /// — exercises the "app doesn't implement rollback" debug path.
    #[test]
    fn test_snapshot_before_outgoing_default_returns_none() {
        let router = LrgpRouter::new();
        router.register(Box::new(MockGame));
        let snap = router.snapshot_before_outgoing("mock", "sess1", "local");
        assert!(snap.is_none());
    }

    #[test]
    fn test_snapshot_before_outgoing_unknown_app_returns_none() {
        let router = LrgpRouter::new();
        let snap = router.snapshot_before_outgoing("nope", "sess1", "local");
        assert!(snap.is_none());
    }

    #[test]
    fn test_rollback_outgoing_unknown_app_errors() {
        let router = LrgpRouter::new();
        let result = router.rollback_outgoing("nope", "sess1", "local", None);
        assert!(matches!(result, Err(LrgpError::UnknownApp(_))));
    }

    #[test]
    fn test_rollback_outgoing_default_is_noop() {
        let router = LrgpRouter::new();
        router.register(Box::new(MockGame));
        // None snapshot through the default impl is just an Ok(()) no-op.
        let result = router.rollback_outgoing("mock", "sess1", "local", None);
        assert!(result.is_ok());
    }

    /// Mock with a real snapshot/rollback implementation to verify the
    /// transactional round-trip. The store is shared via Arc so the test can
    /// inspect/mutate state independently of the trait-object inside the router.
    #[derive(Clone)]
    struct SnapshotMock {
        store: Arc<Mutex<HashMap<String, Session>>>,
    }
    impl SnapshotMock {
        fn new() -> Self { Self { store: Arc::new(Mutex::new(HashMap::new())) } }
        fn put(&self, session_id: &str, status: &str) {
            let s = Session {
                session_id: session_id.to_string(),
                identity_id: "local".to_string(),
                app_id: "snap".to_string(),
                app_version: 1,
                contact_hash: String::new(),
                initiator: String::new(),
                status: status.to_string(),
                metadata: HashMap::new(),
                unread: 0,
                created_at: 0.0,
                updated_at: 0.0,
                last_action_at: 0.0,
            };
            self.store.lock().unwrap().insert(session_id.to_string(), s);
        }
        fn get(&self, session_id: &str) -> Option<Session> {
            self.store.lock().unwrap().get(session_id).cloned()
        }
    }
    impl GameApp for SnapshotMock {
        fn app_id(&self) -> &str { "snap" }
        fn version(&self) -> u32 { 1 }
        fn manifest(&self) -> AppManifest {
            AppManifest {
                app_id: "snap".into(), version: 1,
                display_name: "Snap".into(), icon: "".into(),
                session_type: SESSION_TURN_BASED.into(), max_players: 2,
                validation: VALIDATION_BOTH.into(),
                actions: vec![], preferred_delivery: HashMap::new(), ttl: HashMap::new(),
            }
        }
        fn handle_incoming(&self, _: &str, _: &str, _: &HashMap<String, rmpv::Value>, _: &str, _: &str) -> IncomingResult {
            IncomingResult { session: None, emit: None, error: None }
        }
        fn handle_outgoing(&self, _: &str, _: &str, _: &HashMap<String, rmpv::Value>, _: &str) -> OutgoingResult {
            OutgoingResult { payload: HashMap::new(), fallback_text: String::new() }
        }
        fn validate_action(&self, _: &str, _: &str, _: &HashMap<String, rmpv::Value>, _: &str) -> (bool, Option<String>) { (true, None) }
        fn get_session_state(&self, _: &str, _: &str) -> HashMap<String, JsonValue> { HashMap::new() }
        fn render_fallback(&self, _: &str, _: &HashMap<String, rmpv::Value>) -> String { String::new() }
        fn snapshot_session(&self, session_id: &str, _identity_id: &str) -> Option<Session> {
            self.store.lock().unwrap().get(session_id).cloned()
        }
        fn rollback_session(&self, session_id: &str, _identity_id: &str, snapshot: Option<Session>) {
            let mut store = self.store.lock().unwrap();
            match snapshot {
                Some(s) => { store.insert(session_id.to_string(), s); }
                None => { store.remove(session_id); }
            }
        }
    }

    #[test]
    fn test_snapshot_rollback_roundtrip() {
        let router = LrgpRouter::new();
        let mock = SnapshotMock::new();
        mock.put("g1", "active");
        router.register(Box::new(mock.clone()));

        // Snapshot the "active" state, then mutate to "completed".
        let snap = router.snapshot_before_outgoing("snap", "g1", "local");
        assert_eq!(snap.as_ref().unwrap().status, "active");
        mock.put("g1", "completed");
        assert_eq!(mock.get("g1").unwrap().status, "completed");

        // Roll back — state returns to "active".
        router.rollback_outgoing("snap", "g1", "local", snap).unwrap();
        assert_eq!(mock.get("g1").unwrap().status, "active");
    }

    #[test]
    fn test_rollback_with_none_deletes_fresh_session() {
        let router = LrgpRouter::new();
        let mock = SnapshotMock::new();
        mock.put("g1", "pending");
        router.register(Box::new(mock.clone()));

        // Pretend the dispatch created g1 (no prior snapshot). Passing None
        // tells rollback_session to delete the fresh session.
        router.rollback_outgoing("snap", "g1", "local", None).unwrap();
        assert!(mock.get("g1").is_none());
    }
}
