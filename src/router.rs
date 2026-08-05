//! LRGP game router — registry for game implementations and dispatch of
//! incoming/outgoing game messages.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::app_base::{
    AppManifest, GameApp, IncomingDispatch, OutgoingResult, PreparedOutgoing, RemoteProtocolError,
};
use crate::constants::*;
use crate::dedup::{DedupVerdict, ReplayDedup};
use crate::envelope::{self, Envelope, ValidatedEnvelope};
use crate::errors::LrgpError;
use crate::session::{Session, SessionStateMachine};

/// Thread-safe registry of LRGP game implementations.
pub struct LrgpRouter {
    apps: Mutex<HashMap<String, Arc<dyn GameApp>>>,
    dedup: Mutex<ReplayDedup>,
    session_creation: Mutex<()>,
}

impl LrgpRouter {
    pub fn new() -> Self {
        Self {
            apps: Mutex::new(HashMap::new()),
            dedup: Mutex::new(ReplayDedup::new()),
            session_creation: Mutex::new(()),
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
    ///
    /// `sender_hash` is a transport trust boundary: callers MUST derive it
    /// from authenticated LXMF/Reticulum delivery metadata, never from
    /// fallback text, envelope payload, or a display name. LRGP binds that
    /// authenticated value to the session and rejects later mismatches, but
    /// this transport-independent crate cannot authenticate an arbitrary
    /// caller-supplied string itself.
    pub fn dispatch_incoming(
        &self,
        envelope: &Envelope,
        sender_hash: &str,
        identity_id: &str,
    ) -> Result<IncomingDispatch, LrgpError> {
        if sender_hash.trim().is_empty() {
            return Err(LrgpError::AuthenticatedSenderRequired);
        }
        if identity_id.trim().is_empty() {
            return Err(LrgpError::ReceivingIdentityRequired);
        }
        let validated = envelope::validate_envelope(envelope)?;
        let app = self.resolve_app(&validated)?;
        validate_error_payload(&validated)?;

        // Probe replay state after structural/app validation but before
        // participant authorization. A fresh probe deliberately does not
        // record or evict anything until authorization succeeds.
        if self
            .dedup
            .lock()
            .unwrap()
            .probe_scoped(identity_id, envelope)
            == DedupVerdict::Replay
        {
            return Ok(IncomingDispatch::Replay);
        }

        // Session lookup, admission, and creation must be one serialized
        // operation across apps. Re-checking under this guard prevents two
        // concurrent challenges from racing into the same global SID or a
        // retry from being counted as a new challenge at the quota boundary.
        let _challenge_guard = if validated.command == CMD_CHALLENGE {
            Some(self.session_creation.lock().unwrap())
        } else {
            None
        };

        // Authentication failure never records the nonce, so an attacker
        // cannot reserve it or evict legitimate replay entries.
        app.authorize_incoming(
            &validated.session_id,
            &validated.command,
            sender_hash,
            identity_id,
        )?;

        // Atomically record after authorization. A second check is necessary:
        // concurrent copies can both pass the non-recording probe, but exactly
        // one may win this insertion and reach application mutation.
        if self
            .dedup
            .lock()
            .unwrap()
            .check_scoped(identity_id, envelope)
            == DedupVerdict::Replay
        {
            return Ok(IncomingDispatch::Replay);
        }

        if validated.command == CMD_CHALLENGE
            && let Some((owner_app, _)) =
                self.find_session_owner(&validated.session_id, identity_id)
            && owner_app != validated.app_id
        {
            return Err(LrgpError::SessionExists(validated.session_id));
        }

        if validated.command == CMD_ERROR {
            return Ok(IncomingDispatch::RemoteError(RemoteProtocolError {
                app_id: validated.app_id,
                session_id: validated.session_id,
                code: envelope::value_as_str(
                    validated
                        .payload
                        .get(KEY_ERR_CODE)
                        .expect("error payload validated above"),
                )
                .expect("error code type validated above")
                .to_string(),
                message: envelope::value_as_str(
                    validated
                        .payload
                        .get(KEY_ERR_MSG)
                        .expect("error payload validated above"),
                )
                .expect("error message type validated above")
                .to_string(),
                reference: envelope::value_as_str(
                    validated
                        .payload
                        .get(KEY_ERR_REF)
                        .expect("error payload validated above"),
                )
                .expect("error reference type validated above")
                .to_string(),
            }));
        }

        // Existing same-peer challenges bypass the quota and are handled
        // idempotently below. This lookup is deliberately under the global
        // challenge guard acquired above.
        if validated.command == CMD_CHALLENGE
            && app
                .get_session_record(&validated.session_id, identity_id)
                .is_none()
        {
            self.enforce_challenge_admission(identity_id, sender_hash)?;
        }

        let snapshot = app.snapshot_session(&validated.session_id, identity_id);
        let mut result = app.handle_incoming(
            &validated.session_id,
            &validated.command,
            &validated.payload,
            sender_hash,
            identity_id,
        );
        if let Some(error) = result.error.as_mut() {
            // App validation errors are observational: they must not leave a
            // partially-mutated session in live memory.
            app.rollback_session(&validated.session_id, identity_id, snapshot);
            error
                .entry(KEY_ERR_REF.into())
                .or_insert_with(|| serde_json::Value::String(validated.command.clone()));
        }

        Ok(IncomingDispatch::Applied(result))
    }

    /// Dispatch an outgoing action for an already participant-bound session.
    ///
    /// New challenges require [`dispatch_outgoing_to`](Self::dispatch_outgoing_to)
    /// so the expected remote peer is bound before a response can be accepted.
    pub fn dispatch_outgoing(
        &self,
        app_id: &str,
        version: u32,
        command: &str,
        session_id: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
    ) -> Result<PreparedOutgoing, LrgpError> {
        self.prepare_outgoing(
            app_id,
            version,
            command,
            session_id,
            payload,
            identity_id,
            None,
        )
    }

    /// Dispatch an outgoing action while asserting its remote recipient.
    /// This is the required entry point for `challenge`.
    #[allow(clippy::too_many_arguments)]
    pub fn dispatch_outgoing_to(
        &self,
        app_id: &str,
        version: u32,
        command: &str,
        session_id: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
        recipient_hash: &str,
    ) -> Result<PreparedOutgoing, LrgpError> {
        if recipient_hash.trim().is_empty() {
            return Err(LrgpError::ParticipantRequired);
        }
        self.prepare_outgoing(
            app_id,
            version,
            command,
            session_id,
            payload,
            identity_id,
            Some(recipient_hash),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_outgoing(
        &self,
        app_id: &str,
        version: u32,
        command: &str,
        session_id: &str,
        payload: &HashMap<String, rmpv::Value>,
        identity_id: &str,
        recipient_hash: Option<&str>,
    ) -> Result<PreparedOutgoing, LrgpError> {
        if identity_id.trim().is_empty() {
            return Err(LrgpError::OutgoingIdentityRequired);
        }
        if command == CMD_CHALLENGE && recipient_hash.is_none() {
            return Err(LrgpError::ParticipantRequired);
        }

        let effective_session_id = if command == CMD_CHALLENGE && session_id.is_empty() {
            envelope::generate_session_id()
        } else {
            session_id.to_string()
        };

        let provisional = envelope::pack_envelope(
            app_id,
            version,
            command,
            &effective_session_id,
            Some(payload.clone()),
            None,
        )?;
        let validated = envelope::validate_envelope(&provisional)?;
        let app = self.resolve_app(&validated)?;
        validate_error_payload(&validated)?;

        // Use the same global creation guard as inbound challenges so a local
        // challenge cannot race another app (or a remote challenge) into the
        // same `(local identity, SID)` namespace.
        let _challenge_guard = if command == CMD_CHALLENGE {
            Some(self.session_creation.lock().unwrap())
        } else {
            None
        };

        if command == CMD_CHALLENGE
            && self
                .find_session_owner(&effective_session_id, identity_id)
                .is_some()
        {
            return Err(LrgpError::SessionExists(effective_session_id));
        }

        if command != CMD_CHALLENGE {
            let session = app
                .get_session_record(&effective_session_id, identity_id)
                .ok_or_else(|| LrgpError::SessionNotFound(effective_session_id.clone()))?;
            if session.status == STATUS_EXPIRED {
                return Err(LrgpError::SessionExpired(effective_session_id));
            }
            if session.contact_hash.is_empty() {
                return Err(LrgpError::ParticipantRequired);
            }
            if let Some(recipient) = recipient_hash
                && recipient != session.contact_hash
            {
                return Err(LrgpError::UnauthorizedPeer {
                    session_id: session.session_id,
                });
            }

            if command != CMD_ERROR {
                let mut transition_probe = session;
                SessionStateMachine::apply_command(&mut transition_probe, command, false)?;
            }
        }

        if command != CMD_ERROR {
            let (valid, message) =
                app.validate_outgoing_action(&effective_session_id, command, payload, identity_id);
            if !valid {
                let message = message.unwrap_or_else(|| "action rejected".into());
                let code = if message.to_ascii_lowercase().contains("expired") {
                    ERR_SESSION_EXPIRED
                } else if message.to_ascii_lowercase().contains("turn") {
                    ERR_NOT_YOUR_TURN
                } else {
                    ERR_INVALID_MOVE
                };
                return Err(LrgpError::Validation {
                    code: code.into(),
                    message,
                });
            }
        }

        let snapshot = app.snapshot_session(&effective_session_id, identity_id);
        let result: OutgoingResult = if command == CMD_ERROR {
            OutgoingResult {
                payload: payload.clone(),
                fallback_text: "[LRGP] Protocol error".into(),
            }
        } else {
            app.handle_outgoing(&effective_session_id, command, payload, identity_id)
        };

        if command == CMD_CHALLENGE
            && let Err(error) = app.bind_session_peer(
                &effective_session_id,
                identity_id,
                recipient_hash.expect("challenge recipient checked above"),
            )
        {
            app.rollback_session(&effective_session_id, identity_id, snapshot);
            return Err(error);
        }

        let final_envelope = envelope::pack_envelope(
            app_id,
            version,
            command,
            &effective_session_id,
            Some(result.payload),
            None,
        );
        let final_envelope = match final_envelope {
            Ok(envelope) => envelope,
            Err(error) => {
                app.rollback_session(&effective_session_id, identity_id, snapshot);
                return Err(error);
            }
        };

        Ok(PreparedOutgoing {
            envelope: final_envelope,
            session_id: effective_session_id,
            fallback_text: result.fallback_text,
            delivery_method: app.get_delivery_method(command),
        })
    }

    fn resolve_app(&self, envelope: &ValidatedEnvelope) -> Result<Arc<dyn GameApp>, LrgpError> {
        let app = self
            .apps
            .lock()
            .unwrap()
            .get(&envelope.app_id)
            .cloned()
            .ok_or_else(|| LrgpError::UnknownApp(envelope.app_id.clone()))?;
        if envelope.version != app.version() {
            return Err(LrgpError::UnsupportedVersion {
                app_id: envelope.app_id.clone(),
                received: envelope.version,
                supported: app.version(),
            });
        }
        let manifest = app.manifest();
        if envelope.command != CMD_ERROR
            && !manifest
                .actions
                .iter()
                .any(|action| action == &envelope.command)
        {
            return Err(LrgpError::UnsupportedAction {
                app_id: envelope.app_id.clone(),
                command: envelope.command.clone(),
            });
        }
        Ok(app)
    }

    fn enforce_challenge_admission(
        &self,
        identity_id: &str,
        sender_hash: &str,
    ) -> Result<(), LrgpError> {
        let apps: Vec<Arc<dyn GameApp>> = self.apps.lock().unwrap().values().cloned().collect();
        let mut total_pending = 0usize;
        let mut participant_pending = 0usize;
        for app in apps {
            for session in app.list_session_records(Some(identity_id)) {
                if session.status != STATUS_PENDING {
                    continue;
                }
                total_pending += 1;
                if session.contact_hash == sender_hash {
                    participant_pending += 1;
                }
            }
        }
        if participant_pending >= PENDING_SESSIONS_PER_PARTICIPANT_MAX {
            return Err(LrgpError::AdmissionLimit {
                scope: "participant",
                limit: PENDING_SESSIONS_PER_PARTICIPANT_MAX,
            });
        }
        if total_pending >= PENDING_SESSIONS_PER_IDENTITY_MAX {
            return Err(LrgpError::AdmissionLimit {
                scope: "identity",
                limit: PENDING_SESSIONS_PER_IDENTITY_MAX,
            });
        }
        Ok(())
    }

    fn find_session_owner(&self, session_id: &str, identity_id: &str) -> Option<(String, Session)> {
        let apps: Vec<(String, Arc<dyn GameApp>)> = self
            .apps
            .lock()
            .unwrap()
            .iter()
            .map(|(app_id, app)| (app_id.clone(), Arc::clone(app)))
            .collect();
        apps.into_iter().find_map(|(app_id, app)| {
            app.get_session_record(session_id, identity_id)
                .map(|session| (app_id, session))
        })
    }

    /// Restore one persisted session into its registered game implementation.
    pub fn restore_session(&self, session: Session) -> Result<(), LrgpError> {
        let app = self
            .apps
            .lock()
            .unwrap()
            .get(&session.app_id)
            .cloned()
            .ok_or_else(|| LrgpError::UnknownApp(session.app_id.clone()))?;
        if session.app_version != app.version() {
            return Err(LrgpError::UnsupportedVersion {
                app_id: session.app_id.clone(),
                received: session.app_version,
                supported: app.version(),
            });
        }
        if !envelope::is_valid_session_id(&session.session_id) {
            return Err(LrgpError::InvalidEnvelope(
                "restored session id must be exactly 16 lowercase hexadecimal characters".into(),
            ));
        }
        // Coordinate hydration with live challenge creation so the global
        // `(local identity, SID)` uniqueness check and upsert are atomic.
        let _creation_guard = self.session_creation.lock().unwrap();
        if let Some((owner_app, _)) =
            self.find_session_owner(&session.session_id, &session.identity_id)
            && owner_app != session.app_id
        {
            return Err(LrgpError::SessionExists(session.session_id));
        }
        app.upsert_session(session)
    }

    /// Remove one live session and its replay cache.
    pub fn remove_session(
        &self,
        app_id: &str,
        session_id: &str,
        identity_id: &str,
    ) -> Result<bool, LrgpError> {
        // Keep deletion atomic with challenge creation/hydration in the same
        // global session namespace.
        let _creation_guard = self.session_creation.lock().unwrap();
        let app = self
            .apps
            .lock()
            .unwrap()
            .get(app_id)
            .cloned()
            .ok_or_else(|| LrgpError::UnknownApp(app_id.into()))?;
        let removed = app.remove_session(session_id, identity_id);
        if removed {
            self.dedup
                .lock()
                .unwrap()
                .drop_scoped_session(identity_id, session_id);
        }
        Ok(removed)
    }

    pub fn list_sessions(
        &self,
        app_id: &str,
        identity_id: Option<&str>,
    ) -> Result<Vec<Session>, LrgpError> {
        let app = self
            .apps
            .lock()
            .unwrap()
            .get(app_id)
            .cloned()
            .ok_or_else(|| LrgpError::UnknownApp(app_id.into()))?;
        Ok(app.list_session_records(identity_id))
    }

    /// Snapshot current in-memory session state before an external transaction.
    ///
    /// Returns `None` for unknown apps, apps that don't implement rollback
    /// (the [`GameApp::snapshot_session`] default returns `None`), or sessions
    /// that don't yet exist. The recommended transactional pattern is:
    ///
    /// ```ignore
    /// let snap = router.snapshot_session(app_id, session_id, identity_id);
    /// let prepared = router.dispatch_outgoing(...)?;
    /// match send_lxmf(prepared.envelope, prepared.fallback_text) {
    ///     Ok(_) => { /* committed */ }
    ///     Err(_) => router.rollback_outgoing(app_id, session_id, identity_id, snap)?,
    /// }
    /// ```
    pub fn snapshot_session(
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
                    "snapshot_session: unknown app \u{2014} transaction will be unrollbackable"
                );
                return None;
            }
        };
        let snap = app.snapshot_session(session_id, identity_id);
        if snap.is_none() {
            tracing::debug!(
                app_id,
                session_id,
                "snapshot_session: no snapshot (new session, or app does not implement rollback)"
            );
        }
        snap
    }

    /// Compatibility alias for outbound integrations. New code should use
    /// [`snapshot_session`](Self::snapshot_session), which also describes
    /// durable inbound transactions accurately.
    pub fn snapshot_before_outgoing(
        &self,
        app_id: &str,
        session_id: &str,
        identity_id: &str,
    ) -> Option<Session> {
        self.snapshot_session(app_id, session_id, identity_id)
    }

    /// Reverse a [`dispatch_outgoing`](Self::dispatch_outgoing) mutation
    /// after a failed LXMF send. `Some(session)` restores prior state via
    /// [`GameApp::rollback_session`]; `None` means the dispatch created a
    /// fresh session that should now be deleted (the app decides how).
    /// See [`snapshot_session`](Self::snapshot_session) for
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

    /// Reverse one successfully applied inbound mutation after its external
    /// durable transaction fails.
    ///
    /// This restores `snapshot` (or deletes a session freshly created by the
    /// dispatch when it is `None`) and releases only the accepted replay key
    /// `(identity_id, session_id, nonce)`. The exact same authenticated
    /// envelope can therefore be retried without weakening replay protection
    /// for any other envelope, session, or receiving identity.
    ///
    /// Call this only for the exact [`IncomingDispatch::Applied`] envelope
    /// whose durable commit failed. Do not use it for a replay, a remote
    /// protocol error, or an application rejection. The caller should retain
    /// the pre-dispatch snapshot and the nonce returned by canonical envelope
    /// validation:
    ///
    /// ```ignore
    /// let validated = validate_envelope(&envelope)?;
    /// let snapshot = router.snapshot_session(app_id, session_id, identity_id);
    /// let applied = router.dispatch_incoming(&envelope, authenticated_sender, identity_id)?;
    /// if durable_commit(&applied).is_err() {
    ///     router.rollback_incoming(
    ///         app_id,
    ///         session_id,
    ///         identity_id,
    ///         &validated.nonce,
    ///         snapshot,
    ///     )?;
    /// }
    /// ```
    pub fn rollback_incoming(
        &self,
        app_id: &str,
        session_id: &str,
        identity_id: &str,
        nonce: &[u8; NONCE_BYTES],
        snapshot: Option<Session>,
    ) -> Result<(), LrgpError> {
        let app = self
            .apps
            .lock()
            .unwrap()
            .get(app_id)
            .cloned()
            .ok_or_else(|| LrgpError::UnknownApp(app_id.to_string()))?;

        // Coordinate a fresh-session delete/restore with challenge creation
        // and hydration. Holding the replay lock across state restoration
        // ensures a subsequent inbound dispatch cannot observe the nonce as
        // retryable until the corresponding state has actually been restored.
        let _creation_guard = self.session_creation.lock().unwrap();
        let mut dedup = self.dedup.lock().unwrap();
        app.rollback_session(session_id, identity_id, snapshot);
        dedup.forget_scoped_nonce(identity_id, session_id, nonce);
        Ok(())
    }

    /// Release one accepted inbound replay key without changing application
    /// state.
    ///
    /// This is the narrow recovery path for an authenticated
    /// [`IncomingDispatch::RemoteError`] whose external durable record could
    /// not be committed: remote errors consume a nonce but do not mutate the
    /// game session, so restoring a session snapshot would be incorrect. Only
    /// the exact `(identity_id, session_id, nonce)` observation is forgotten;
    /// all other replay protection remains intact.
    pub fn forget_incoming_nonce(
        &self,
        identity_id: &str,
        session_id: &str,
        nonce: &[u8; NONCE_BYTES],
    ) {
        self.dedup
            .lock()
            .unwrap()
            .forget_scoped_nonce(identity_id, session_id, nonce);
    }
}

fn validate_error_payload(envelope: &ValidatedEnvelope) -> Result<(), LrgpError> {
    if envelope.command != CMD_ERROR {
        return Ok(());
    }
    let required = [KEY_ERR_CODE, KEY_ERR_MSG, KEY_ERR_REF];
    if envelope.payload.len() != required.len()
        || !envelope
            .payload
            .keys()
            .all(|key| required.contains(&key.as_str()))
    {
        return Err(LrgpError::InvalidEnvelope(
            "error payload must contain exactly code, msg, and ref".into(),
        ));
    }
    for key in required {
        match envelope.payload.get(key).and_then(envelope::value_as_str) {
            Some(value) if !value.is_empty() => {}
            _ => {
                return Err(LrgpError::InvalidEnvelope(format!(
                    "error payload key '{key}' must be a non-empty string"
                )));
            }
        }
    }
    Ok(())
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
    use crate::apps::chess::ChessApp;
    use crate::apps::tictactoe::TicTacToeApp;
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
        fn bind_session_peer(
            &self,
            _session_id: &str,
            _identity_id: &str,
            _peer_hash: &str,
        ) -> Result<(), LrgpError> {
            Ok(())
        }
        fn authorize_incoming(
            &self,
            _session_id: &str,
            _command: &str,
            _sender_hash: &str,
            _identity_id: &str,
        ) -> Result<(), LrgpError> {
            Ok(())
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

        let env = envelope::pack_envelope("mock", 1, "challenge", "0000000000000001", None, None)
            .unwrap();
        let result = router.dispatch_incoming(&env, "sender", "local").unwrap();
        let IncomingDispatch::Applied(result) = result else {
            panic!("fresh envelope must be applied");
        };
        assert!(result.error.is_none());
        assert!(result.emit.is_some());
    }

    #[test]
    fn test_dispatch_incoming_unknown_app() {
        let router = LrgpRouter::new();
        let env =
            envelope::pack_envelope("unknown", 1, "challenge", "0000000000000001", None, None)
                .unwrap();
        let result = router.dispatch_incoming(&env, "sender", "local");
        assert!(matches!(result, Err(LrgpError::UnknownApp(_))));
    }

    #[test]
    fn incoming_dispatch_requires_both_transport_identities_without_consuming_nonce() {
        let router = LrgpRouter::new();
        router.register(Box::new(TicTacToeApp::new()));
        let sid = "00000000000000b3";
        let challenge = envelope::pack_envelope(
            "ttt",
            1,
            CMD_CHALLENGE,
            sid,
            None,
            Some([0x63; NONCE_BYTES]),
        )
        .unwrap();

        assert!(matches!(
            router.dispatch_incoming(&challenge, "", "local"),
            Err(LrgpError::AuthenticatedSenderRequired)
        ));
        assert!(matches!(
            router.dispatch_incoming(&challenge, "remote", "  "),
            Err(LrgpError::ReceivingIdentityRequired)
        ));
        assert!(router.list_sessions("ttt", None).unwrap().is_empty());

        // Neither failed trust-boundary check reserved the envelope nonce.
        assert!(matches!(
            router.dispatch_incoming(&challenge, "remote", "local"),
            Ok(IncomingDispatch::Applied(_))
        ));
        assert_eq!(router.list_sessions("ttt", Some("local")).unwrap().len(), 1);
    }

    #[test]
    fn test_dispatch_outgoing() {
        let router = LrgpRouter::new();
        router.register(Box::new(MockGame));

        let prepared = router
            .dispatch_outgoing_to(
                "mock",
                1,
                "challenge",
                "0000000000000001",
                &HashMap::new(),
                "local",
                "remote",
            )
            .unwrap();
        assert!(prepared.envelope.contains_key(KEY_APP));
        assert_eq!(prepared.fallback_text, "[LRGP Mock] challenge");
        assert_eq!(prepared.delivery_method, "opportunistic");
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
        fn new() -> Self {
            Self {
                store: Arc::new(Mutex::new(HashMap::new())),
            }
        }
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
        fn app_id(&self) -> &str {
            "snap"
        }
        fn version(&self) -> u32 {
            1
        }
        fn manifest(&self) -> AppManifest {
            AppManifest {
                app_id: "snap".into(),
                version: 1,
                display_name: "Snap".into(),
                icon: "".into(),
                session_type: SESSION_TURN_BASED.into(),
                max_players: 2,
                validation: VALIDATION_BOTH.into(),
                actions: vec![CMD_CHALLENGE.into(), CMD_MOVE.into(), CMD_ERROR.into()],
                preferred_delivery: HashMap::new(),
                ttl: HashMap::new(),
            }
        }
        fn handle_incoming(
            &self,
            session_id: &str,
            command: &str,
            _: &HashMap<String, rmpv::Value>,
            _: &str,
            _: &str,
        ) -> IncomingResult {
            if command == CMD_MOVE {
                if let Some(mut session) = self.get(session_id) {
                    session.status = STATUS_COMPLETED.into();
                    self.store
                        .lock()
                        .unwrap()
                        .insert(session_id.into(), session);
                }
                let mut error = HashMap::new();
                error.insert(
                    KEY_ERR_CODE.into(),
                    JsonValue::String(ERR_INVALID_MOVE.into()),
                );
                error.insert(
                    KEY_ERR_MSG.into(),
                    JsonValue::String("rejected after provisional mutation".into()),
                );
                return IncomingResult {
                    session: None,
                    emit: None,
                    error: Some(error),
                };
            }
            IncomingResult {
                session: None,
                emit: None,
                error: None,
            }
        }
        fn handle_outgoing(
            &self,
            _: &str,
            _: &str,
            _: &HashMap<String, rmpv::Value>,
            _: &str,
        ) -> OutgoingResult {
            OutgoingResult {
                payload: HashMap::new(),
                fallback_text: String::new(),
            }
        }
        fn validate_action(
            &self,
            _: &str,
            _: &str,
            _: &HashMap<String, rmpv::Value>,
            _: &str,
        ) -> (bool, Option<String>) {
            (true, None)
        }
        fn get_session_state(&self, _: &str, _: &str) -> HashMap<String, JsonValue> {
            HashMap::new()
        }
        fn render_fallback(&self, _: &str, _: &HashMap<String, rmpv::Value>) -> String {
            String::new()
        }
        fn snapshot_session(&self, session_id: &str, _identity_id: &str) -> Option<Session> {
            self.store.lock().unwrap().get(session_id).cloned()
        }
        fn authorize_incoming(
            &self,
            _session_id: &str,
            _command: &str,
            _sender_hash: &str,
            _identity_id: &str,
        ) -> Result<(), LrgpError> {
            Ok(())
        }
        fn rollback_session(
            &self,
            session_id: &str,
            _identity_id: &str,
            snapshot: Option<Session>,
        ) {
            let mut store = self.store.lock().unwrap();
            match snapshot {
                Some(s) => {
                    store.insert(session_id.to_string(), s);
                }
                None => {
                    store.remove(session_id);
                }
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
        router
            .rollback_outgoing("snap", "g1", "local", snap)
            .unwrap();
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
        router
            .rollback_outgoing("snap", "g1", "local", None)
            .unwrap();
        assert!(mock.get("g1").is_none());
    }

    #[test]
    fn rollback_incoming_restores_state_and_allows_exact_retransmit() {
        let router = LrgpRouter::new();
        router.register(Box::new(TicTacToeApp::new()));
        let sid = "00000000000000a1";
        router
            .dispatch_outgoing_to(
                "ttt",
                1,
                CMD_CHALLENGE,
                sid,
                &HashMap::new(),
                "alice",
                "bob",
            )
            .unwrap();

        let snapshot = router.snapshot_session("ttt", sid, "alice");
        assert_eq!(snapshot.as_ref().unwrap().status, STATUS_PENDING);
        let nonce = [0x55; NONCE_BYTES];
        let accept = envelope::pack_envelope(
            "ttt",
            1,
            CMD_ACCEPT,
            sid,
            Some(HashMap::from([
                ("b".into(), rmpv::Value::String("_________".into())),
                ("t".into(), rmpv::Value::String("alice".into())),
            ])),
            Some(nonce),
        )
        .unwrap();

        assert!(matches!(
            router.dispatch_incoming(&accept, "bob", "alice"),
            Ok(IncomingDispatch::Applied(_))
        ));
        assert_eq!(
            router.snapshot_session("ttt", sid, "alice").unwrap().status,
            STATUS_ACTIVE
        );

        router
            .rollback_incoming("ttt", sid, "alice", &nonce, snapshot)
            .unwrap();
        assert_eq!(
            router.snapshot_session("ttt", sid, "alice").unwrap().status,
            STATUS_PENDING
        );

        // The exact same envelope is fresh again only after both live state
        // and its precise replay key have been rolled back.
        assert!(matches!(
            router.dispatch_incoming(&accept, "bob", "alice"),
            Ok(IncomingDispatch::Applied(_))
        ));
        assert_eq!(
            router.snapshot_session("ttt", sid, "alice").unwrap().status,
            STATUS_ACTIVE
        );
        assert!(matches!(
            router.dispatch_incoming(&accept, "bob", "alice"),
            Ok(IncomingDispatch::Replay)
        ));
    }

    #[test]
    fn rollback_incoming_none_deletes_new_session_before_retry() {
        let router = LrgpRouter::new();
        router.register(Box::new(TicTacToeApp::new()));
        let sid = "00000000000000a2";
        let nonce = [0x56; NONCE_BYTES];
        let challenge =
            envelope::pack_envelope("ttt", 1, CMD_CHALLENGE, sid, None, Some(nonce)).unwrap();

        assert!(router.snapshot_session("ttt", sid, "bob").is_none());
        assert!(matches!(
            router.dispatch_incoming(&challenge, "alice", "bob"),
            Ok(IncomingDispatch::Applied(_))
        ));
        assert!(router.snapshot_session("ttt", sid, "bob").is_some());

        router
            .rollback_incoming("ttt", sid, "bob", &nonce, None)
            .unwrap();
        assert!(router.snapshot_session("ttt", sid, "bob").is_none());
        assert!(matches!(
            router.dispatch_incoming(&challenge, "alice", "bob"),
            Ok(IncomingDispatch::Applied(_))
        ));
    }

    #[test]
    fn rollback_incoming_forgets_only_the_named_identity_scope() {
        let router = LrgpRouter::new();
        router.register(Box::new(MockGame));
        let sid = "00000000000000a3";
        let nonce = [0x57; NONCE_BYTES];
        let challenge =
            envelope::pack_envelope("mock", 1, CMD_CHALLENGE, sid, None, Some(nonce)).unwrap();

        for identity in ["local-a", "local-b"] {
            assert!(matches!(
                router.dispatch_incoming(&challenge, "remote", identity),
                Ok(IncomingDispatch::Applied(_))
            ));
        }

        router
            .rollback_incoming("mock", sid, "local-a", &nonce, None)
            .unwrap();
        assert!(matches!(
            router.dispatch_incoming(&challenge, "remote", "local-a"),
            Ok(IncomingDispatch::Applied(_))
        ));
        assert!(matches!(
            router.dispatch_incoming(&challenge, "remote", "local-b"),
            Ok(IncomingDispatch::Replay)
        ));
    }

    #[test]
    fn forget_incoming_nonce_is_scoped_and_does_not_touch_session_state() {
        let router = LrgpRouter::new();
        router.register(Box::new(MockGame));
        let sid = "00000000000000a4";
        let nonce = [0x58; NONCE_BYTES];
        let error = envelope::pack_envelope(
            "mock",
            1,
            CMD_ERROR,
            sid,
            Some(HashMap::from([
                (
                    KEY_ERR_CODE.into(),
                    rmpv::Value::String(ERR_INVALID_MOVE.into()),
                ),
                (KEY_ERR_MSG.into(), rmpv::Value::String("bad move".into())),
                (KEY_ERR_REF.into(), rmpv::Value::String(CMD_MOVE.into())),
            ])),
            Some(nonce),
        )
        .unwrap();

        for identity in ["local-a", "local-b"] {
            assert!(matches!(
                router.dispatch_incoming(&error, "remote", identity),
                Ok(IncomingDispatch::RemoteError(_))
            ));
        }

        router.forget_incoming_nonce("local-a", sid, &nonce);
        assert!(matches!(
            router.dispatch_incoming(&error, "remote", "local-a"),
            Ok(IncomingDispatch::RemoteError(_))
        ));
        assert!(matches!(
            router.dispatch_incoming(&error, "remote", "local-b"),
            Ok(IncomingDispatch::Replay)
        ));
        assert!(router.snapshot_session("mock", sid, "local-a").is_none());
    }

    #[test]
    fn canonical_router_rejects_unsupported_version_and_action() {
        let router = LrgpRouter::new();
        router.register(Box::new(MockGame));
        let sid = "0000000000000001";

        let wrong_version =
            envelope::pack_envelope("mock", 2, CMD_CHALLENGE, sid, None, None).unwrap();
        assert!(matches!(
            router.dispatch_incoming(&wrong_version, "remote", "local"),
            Err(LrgpError::UnsupportedVersion { .. })
        ));

        let unknown_action =
            envelope::pack_envelope("mock", 1, CMD_ACCEPT, sid, None, None).unwrap();
        assert!(matches!(
            router.dispatch_incoming(&unknown_action, "remote", "local"),
            Err(LrgpError::UnsupportedAction { .. })
        ));
    }

    #[test]
    fn router_filters_replay_per_receiving_identity() {
        let router = LrgpRouter::new();
        router.register(Box::new(MockGame));
        let envelope = envelope::pack_envelope(
            "mock",
            1,
            CMD_CHALLENGE,
            "0000000000000002",
            None,
            Some([0x42; NONCE_BYTES]),
        )
        .unwrap();

        assert!(matches!(
            router.dispatch_incoming(&envelope, "remote", "local-a"),
            Ok(IncomingDispatch::Applied(_))
        ));
        assert!(matches!(
            router.dispatch_incoming(&envelope, "remote", "local-a"),
            Ok(IncomingDispatch::Replay)
        ));
        assert!(matches!(
            router.dispatch_incoming(&envelope, "remote", "local-b"),
            Ok(IncomingDispatch::Applied(_))
        ));
    }

    #[test]
    fn challenge_requires_and_binds_remote_participant() {
        let router = LrgpRouter::new();
        router.register(Box::new(TicTacToeApp::new()));
        let sid = "0000000000000003";

        assert!(matches!(
            router.dispatch_outgoing("ttt", 1, CMD_CHALLENGE, sid, &HashMap::new(), "alice"),
            Err(LrgpError::ParticipantRequired)
        ));
        assert!(matches!(
            router.dispatch_outgoing_to(
                "ttt",
                1,
                CMD_CHALLENGE,
                sid,
                &HashMap::new(),
                "   ",
                "bob",
            ),
            Err(LrgpError::OutgoingIdentityRequired)
        ));
        assert!(matches!(
            router.dispatch_outgoing_to(
                "ttt",
                1,
                CMD_CHALLENGE,
                sid,
                &HashMap::new(),
                "alice",
                "  ",
            ),
            Err(LrgpError::ParticipantRequired)
        ));
        assert!(router.list_sessions("ttt", None).unwrap().is_empty());

        router
            .dispatch_outgoing_to(
                "ttt",
                1,
                CMD_CHALLENGE,
                sid,
                &HashMap::new(),
                "alice",
                "bob",
            )
            .unwrap();
        let session = router.snapshot_session("ttt", sid, "alice").unwrap();
        assert_eq!(session.contact_hash, "bob");

        assert!(matches!(
            router.dispatch_outgoing_to(
                "ttt",
                1,
                CMD_CHALLENGE,
                sid,
                &HashMap::new(),
                "alice",
                "bob",
            ),
            Err(LrgpError::SessionExists(existing)) if existing == sid
        ));
    }

    #[test]
    fn session_ids_are_unique_per_local_identity_across_apps() {
        let router = LrgpRouter::new();
        router.register(Box::new(TicTacToeApp::new()));
        router.register(Box::new(ChessApp::new()));
        let sid = "000000000000000a";

        router
            .dispatch_outgoing_to(
                "ttt",
                1,
                CMD_CHALLENGE,
                sid,
                &HashMap::new(),
                "alice",
                "bob",
            )
            .unwrap();

        assert!(matches!(
            router.dispatch_outgoing_to(
                "chess",
                1,
                CMD_CHALLENGE,
                sid,
                &HashMap::new(),
                "alice",
                "bob",
            ),
            Err(LrgpError::SessionExists(existing)) if existing == sid
        ));

        let incoming = envelope::pack_envelope(
            "chess",
            1,
            CMD_CHALLENGE,
            sid,
            None,
            Some([0xa1; NONCE_BYTES]),
        )
        .unwrap();
        assert!(matches!(
            router.dispatch_incoming(&incoming, "bob", "alice"),
            Err(LrgpError::SessionExists(existing)) if existing == sid
        ));
        // The collision was structurally valid and reached the global
        // session guard, so its nonce remains consumed.
        assert!(matches!(
            router.dispatch_incoming(&incoming, "bob", "alice"),
            Ok(IncomingDispatch::Replay)
        ));

        let mut restored = Session::new(sid);
        restored.identity_id = "alice".into();
        restored.app_id = "chess".into();
        restored.app_version = 1;
        restored.contact_hash = "bob".into();
        restored.initiator = "alice".into();
        assert!(matches!(
            router.restore_session(restored),
            Err(LrgpError::SessionExists(existing)) if existing == sid
        ));

        // The namespace is local-identity scoped, so a second local identity
        // may independently use the same random session ID.
        router
            .dispatch_outgoing_to(
                "chess",
                1,
                CMD_CHALLENGE,
                sid,
                &HashMap::new(),
                "carol",
                "dave",
            )
            .unwrap();
    }

    #[test]
    fn concurrent_cross_app_challenges_create_exactly_one_session() {
        let router = Arc::new(LrgpRouter::new());
        router.register(Box::new(TicTacToeApp::new()));
        router.register(Box::new(ChessApp::new()));
        let start = Arc::new(std::sync::Barrier::new(3));
        let sid = "000000000000000b";

        let ttt = {
            let router = Arc::clone(&router);
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                router.dispatch_outgoing_to(
                    "ttt",
                    1,
                    CMD_CHALLENGE,
                    sid,
                    &HashMap::new(),
                    "alice",
                    "bob",
                )
            })
        };
        let chess = {
            let router = Arc::clone(&router);
            let start = Arc::clone(&start);
            std::thread::spawn(move || {
                start.wait();
                router.dispatch_outgoing_to(
                    "chess",
                    1,
                    CMD_CHALLENGE,
                    sid,
                    &HashMap::new(),
                    "alice",
                    "bob",
                )
            })
        };
        start.wait();

        let results = [ttt.join().unwrap(), chess.join().unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(LrgpError::SessionExists(existing)) if existing == sid))
                .count(),
            1
        );
        let live_count = router.list_sessions("ttt", Some("alice")).unwrap().len()
            + router.list_sessions("chess", Some("alice")).unwrap().len();
        assert_eq!(live_count, 1);
    }

    #[test]
    fn authorization_failure_does_not_reserve_legitimate_nonce() {
        let router = LrgpRouter::new();
        router.register(Box::new(TicTacToeApp::new()));
        let sid = "0000000000000004";
        router
            .dispatch_outgoing_to(
                "ttt",
                1,
                CMD_CHALLENGE,
                sid,
                &HashMap::new(),
                "alice",
                "bob",
            )
            .unwrap();
        let accept_payload = HashMap::from([
            ("b".into(), rmpv::Value::String("_________".into())),
            ("t".into(), rmpv::Value::String("alice".into())),
        ]);
        let accept = envelope::pack_envelope(
            "ttt",
            1,
            CMD_ACCEPT,
            sid,
            Some(accept_payload),
            Some([0x44; NONCE_BYTES]),
        )
        .unwrap();

        assert!(matches!(
            router.dispatch_incoming(&accept, "mallory", "alice"),
            Err(LrgpError::UnauthorizedPeer { .. })
        ));
        assert!(matches!(
            router.dispatch_incoming(&accept, "bob", "alice"),
            Ok(IncomingDispatch::Applied(_))
        ));
        assert_eq!(
            router.snapshot_session("ttt", sid, "alice").unwrap().status,
            STATUS_ACTIVE
        );
    }

    #[test]
    fn authorization_failure_cannot_evict_legitimate_replay_nonce() {
        let router = LrgpRouter {
            apps: Mutex::new(HashMap::new()),
            dedup: Mutex::new(ReplayDedup::with_limits(1, 600, 4)),
            session_creation: Mutex::new(()),
        };
        router.register(Box::new(TicTacToeApp::new()));
        let sid = "00000000000000b1";
        router
            .dispatch_outgoing_to(
                "ttt",
                1,
                CMD_CHALLENGE,
                sid,
                &HashMap::new(),
                "alice",
                "bob",
            )
            .unwrap();

        let error_payload = HashMap::from([
            (
                KEY_ERR_CODE.into(),
                rmpv::Value::String(ERR_INVALID_MOVE.into()),
            ),
            (KEY_ERR_MSG.into(), rmpv::Value::String("bad move".into())),
            (KEY_ERR_REF.into(), rmpv::Value::String(CMD_MOVE.into())),
        ]);
        let legitimate = envelope::pack_envelope(
            "ttt",
            1,
            CMD_ERROR,
            sid,
            Some(error_payload.clone()),
            Some([0x61; NONCE_BYTES]),
        )
        .unwrap();
        let unauthorized = envelope::pack_envelope(
            "ttt",
            1,
            CMD_ERROR,
            sid,
            Some(error_payload),
            Some([0x62; NONCE_BYTES]),
        )
        .unwrap();

        assert!(matches!(
            router.dispatch_incoming(&legitimate, "bob", "alice"),
            Ok(IncomingDispatch::RemoteError(_))
        ));
        assert!(matches!(
            router.dispatch_incoming(&unauthorized, "mallory", "alice"),
            Err(LrgpError::UnauthorizedPeer { .. })
        ));
        assert!(matches!(
            router.dispatch_incoming(&legitimate, "bob", "alice"),
            Ok(IncomingDispatch::Replay)
        ));
    }

    #[test]
    fn same_peer_fresh_challenge_is_idempotent_but_other_peer_is_rejected() {
        let router = LrgpRouter::new();
        router.register(Box::new(TicTacToeApp::new()));
        let sid = "0000000000000005";
        let first =
            envelope::pack_envelope("ttt", 1, CMD_CHALLENGE, sid, None, Some([1; NONCE_BYTES]))
                .unwrap();
        let duplicate =
            envelope::pack_envelope("ttt", 1, CMD_CHALLENGE, sid, None, Some([2; NONCE_BYTES]))
                .unwrap();

        let IncomingDispatch::Applied(first_result) =
            router.dispatch_incoming(&first, "alice", "bob").unwrap()
        else {
            panic!("first challenge must apply");
        };
        assert!(first_result.emit.is_some());
        let before = router.snapshot_session("ttt", sid, "bob").unwrap();

        let IncomingDispatch::Applied(duplicate_result) = router
            .dispatch_incoming(&duplicate, "alice", "bob")
            .unwrap()
        else {
            panic!("same-peer retry must be an idempotent application result");
        };
        assert!(duplicate_result.emit.is_none());
        let after = router.snapshot_session("ttt", sid, "bob").unwrap();
        assert_eq!(after.created_at, before.created_at);
        assert_eq!(after.contact_hash, before.contact_hash);

        assert!(matches!(
            router.dispatch_incoming(&duplicate, "mallory", "bob"),
            Ok(IncomingDispatch::Replay)
        ));
        let other_nonce =
            envelope::pack_envelope("ttt", 1, CMD_CHALLENGE, sid, None, Some([3; NONCE_BYTES]))
                .unwrap();
        assert!(matches!(
            router.dispatch_incoming(&other_nonce, "mallory", "bob"),
            Err(LrgpError::UnauthorizedPeer { .. })
        ));
    }

    #[test]
    fn remote_error_is_typed_authenticated_and_never_a_local_rejection() {
        let router = LrgpRouter::new();
        router.register(Box::new(TicTacToeApp::new()));
        let sid = "0000000000000006";
        router
            .dispatch_outgoing_to(
                "ttt",
                1,
                CMD_CHALLENGE,
                sid,
                &HashMap::new(),
                "alice",
                "bob",
            )
            .unwrap();

        let mut payload = HashMap::new();
        payload.insert(
            KEY_ERR_CODE.into(),
            rmpv::Value::String(ERR_INVALID_MOVE.into()),
        );
        payload.insert(KEY_ERR_MSG.into(), rmpv::Value::String("bad move".into()));
        payload.insert(KEY_ERR_REF.into(), rmpv::Value::String(CMD_MOVE.into()));
        let nonce = [6; NONCE_BYTES];
        let error =
            envelope::pack_envelope("ttt", 1, CMD_ERROR, sid, Some(payload), Some(nonce)).unwrap();

        let mut invalid_payload = HashMap::new();
        invalid_payload.insert(
            KEY_ERR_CODE.into(),
            rmpv::Value::String(ERR_INVALID_MOVE.into()),
        );
        invalid_payload.insert(KEY_ERR_MSG.into(), rmpv::Value::String("bad move".into()));
        invalid_payload.insert(KEY_ERR_REF.into(), rmpv::Value::String(CMD_MOVE.into()));
        invalid_payload.insert("extra".into(), rmpv::Value::Boolean(true));
        let invalid_error =
            envelope::pack_envelope("ttt", 1, CMD_ERROR, sid, Some(invalid_payload), Some(nonce))
                .unwrap();
        assert!(matches!(
            router.dispatch_incoming(&invalid_error, "bob", "alice"),
            Err(LrgpError::InvalidEnvelope(_))
        ));

        let IncomingDispatch::RemoteError(remote) =
            router.dispatch_incoming(&error, "bob", "alice").unwrap()
        else {
            panic!("remote error must have a distinct dispatch result");
        };
        assert_eq!(remote.code, ERR_INVALID_MOVE);
        assert_eq!(remote.reference, CMD_MOVE);
        assert_eq!(
            router.snapshot_session("ttt", sid, "alice").unwrap().status,
            STATUS_PENDING
        );
        assert!(matches!(
            router.dispatch_incoming(&error, "bob", "alice"),
            Ok(IncomingDispatch::Replay)
        ));
    }

    #[test]
    fn outgoing_protocol_error_uses_router_validation_not_game_action_validation() {
        let router = LrgpRouter::new();
        router.register(Box::new(TicTacToeApp::new()));
        let sid = "00000000000000b2";
        router
            .dispatch_outgoing_to(
                "ttt",
                1,
                CMD_CHALLENGE,
                sid,
                &HashMap::new(),
                "alice",
                "bob",
            )
            .unwrap();
        let before = router.snapshot_session("ttt", sid, "alice").unwrap();
        let payload = HashMap::from([
            (
                KEY_ERR_CODE.into(),
                rmpv::Value::String(ERR_INVALID_MOVE.into()),
            ),
            (KEY_ERR_MSG.into(), rmpv::Value::String("bad move".into())),
            (KEY_ERR_REF.into(), rmpv::Value::String(CMD_MOVE.into())),
        ]);

        let prepared = router
            .dispatch_outgoing_to("ttt", 1, CMD_ERROR, sid, &payload, "alice", "bob")
            .unwrap();
        let validated = envelope::validate_envelope(&prepared.envelope).unwrap();
        assert_eq!(validated.command, CMD_ERROR);
        assert_eq!(validated.payload, payload);
        assert_eq!(prepared.delivery_method, "opportunistic");
        assert_eq!(
            serde_json::to_value(router.snapshot_session("ttt", sid, "alice").unwrap()).unwrap(),
            serde_json::to_value(before).unwrap()
        );

        let mut invalid = payload;
        invalid.insert("extra".into(), rmpv::Value::Boolean(true));
        assert!(matches!(
            router.dispatch_outgoing_to("ttt", 1, CMD_ERROR, sid, &invalid, "alice", "bob"),
            Err(LrgpError::InvalidEnvelope(_))
        ));
    }

    #[test]
    fn inbound_rejection_rolls_back_provisional_app_mutation() {
        let router = LrgpRouter::new();
        let mock = SnapshotMock::new();
        let sid = "0000000000000007";
        mock.put(sid, STATUS_ACTIVE);
        router.register(Box::new(mock.clone()));
        let action = envelope::pack_envelope("snap", 1, CMD_MOVE, sid, None, None).unwrap();

        let IncomingDispatch::Applied(result) = router
            .dispatch_incoming(&action, "remote", "local")
            .unwrap()
        else {
            panic!("local app rejection is still an applied dispatch result");
        };
        let error = result.error.unwrap();
        assert_eq!(
            error.get(KEY_ERR_REF),
            Some(&JsonValue::String(CMD_MOVE.into()))
        );
        assert_eq!(mock.get(sid).unwrap().status, STATUS_ACTIVE);
    }

    #[test]
    fn restore_applies_ttl_and_remove_clears_live_session() {
        let router = LrgpRouter::new();
        router.register(Box::new(TicTacToeApp::new()));
        let sid = "0000000000000008";
        let mut session = Session::new(sid);
        session.identity_id = "alice".into();
        session.app_id = "ttt".into();
        session.app_version = 1;
        session.contact_hash = "bob".into();
        session.initiator = "alice".into();
        session.last_action_at = 0.0;
        router.restore_session(session).unwrap();

        let sessions = router.list_sessions("ttt", Some("alice")).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, STATUS_EXPIRED);
        assert!(matches!(
            router.dispatch_outgoing_to(
                "ttt",
                1,
                CMD_ACCEPT,
                sid,
                &HashMap::new(),
                "alice",
                "bob",
            ),
            Err(LrgpError::SessionExpired(expired)) if expired == sid
        ));
        assert!(router.remove_session("ttt", sid, "alice").unwrap());
        assert!(
            router
                .list_sessions("ttt", Some("alice"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn delivery_preference_is_returned_with_prepared_action() {
        let router = LrgpRouter::new();
        router.register(Box::new(TicTacToeApp::new()));
        let sid = "0000000000000009";
        router
            .dispatch_outgoing_to(
                "ttt",
                1,
                CMD_CHALLENGE,
                sid,
                &HashMap::new(),
                "alice",
                "bob",
            )
            .unwrap();
        let accept = envelope::pack_envelope(
            "ttt",
            1,
            CMD_ACCEPT,
            sid,
            Some(HashMap::from([
                ("b".into(), rmpv::Value::String("_________".into())),
                ("t".into(), rmpv::Value::String("alice".into())),
            ])),
            None,
        )
        .unwrap();
        router.dispatch_incoming(&accept, "bob", "alice").unwrap();

        let resign = router
            .dispatch_outgoing_to("ttt", 1, CMD_RESIGN, sid, &HashMap::new(), "alice", "bob")
            .unwrap();
        assert_eq!(resign.delivery_method, "direct");
    }

    #[test]
    fn pending_challenge_admission_is_bounded_without_eviction() {
        let router = LrgpRouter::new();
        router.register(Box::new(TicTacToeApp::new()));

        for index in 0..PENDING_SESSIONS_PER_PARTICIPANT_MAX {
            let sid = format!("{index:016x}");
            let challenge =
                envelope::pack_envelope("ttt", 1, CMD_CHALLENGE, &sid, None, None).unwrap();
            assert!(matches!(
                router.dispatch_incoming(&challenge, "spammer", "local"),
                Ok(IncomingDispatch::Applied(_))
            ));
        }
        let over_participant_sid = "0000000000000010";
        let over_participant =
            envelope::pack_envelope("ttt", 1, CMD_CHALLENGE, over_participant_sid, None, None)
                .unwrap();
        assert!(matches!(
            router.dispatch_incoming(&over_participant, "spammer", "local"),
            Err(LrgpError::AdmissionLimit {
                scope: "participant",
                limit: PENDING_SESSIONS_PER_PARTICIPANT_MAX
            })
        ));
        assert_eq!(
            router.list_sessions("ttt", Some("local")).unwrap().len(),
            PENDING_SESSIONS_PER_PARTICIPANT_MAX
        );

        let other =
            envelope::pack_envelope("ttt", 1, CMD_CHALLENGE, over_participant_sid, None, None)
                .unwrap();
        assert!(matches!(
            router.dispatch_incoming(&other, "other", "local"),
            Ok(IncomingDispatch::Applied(_))
        ));
    }

    #[test]
    fn identity_wide_pending_challenge_admission_is_bounded() {
        let router = LrgpRouter::new();
        router.register(Box::new(TicTacToeApp::new()));

        for index in 0..PENDING_SESSIONS_PER_IDENTITY_MAX {
            let sid = format!("{index:016x}");
            let sender = format!("peer-{index}");
            let challenge =
                envelope::pack_envelope("ttt", 1, CMD_CHALLENGE, &sid, None, None).unwrap();
            router
                .dispatch_incoming(&challenge, &sender, "local")
                .unwrap();
        }
        let challenge =
            envelope::pack_envelope("ttt", 1, CMD_CHALLENGE, "0000000000000080", None, None)
                .unwrap();
        assert!(matches!(
            router.dispatch_incoming(&challenge, "new-peer", "local"),
            Err(LrgpError::AdmissionLimit {
                scope: "identity",
                limit: PENDING_SESSIONS_PER_IDENTITY_MAX
            })
        ));
        assert_eq!(
            router.list_sessions("ttt", Some("local")).unwrap().len(),
            PENDING_SESSIONS_PER_IDENTITY_MAX
        );
    }
}
