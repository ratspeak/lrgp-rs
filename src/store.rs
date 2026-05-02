//! LRGP game store — SQLite persistence for game sessions and action history.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use serde_json::Value as JsonValue;

use crate::errors::LrgpError;

/// Allowed columns for session updates (prevents SQL injection).
const ALLOWED_COLUMNS: &[&str] = &[
    "status",
    "metadata",
    "unread",
    "updated_at",
    "last_action_at",
    "contact_hash",
    "initiator",
];

/// A stored game action.
#[derive(Debug, Clone)]
pub struct Action {
    pub session_id: String,
    pub identity_id: String,
    pub action_num: i64,
    pub command: String,
    pub payload_json: String,
    pub sender: String,
    pub timestamp: f64,
}

/// LRGP game store backed by SQLite.
pub struct LrgpStore {
    conn: Mutex<rusqlite::Connection>,
}

impl LrgpStore {
    /// Open (or create) a game store at the given path.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LrgpError> {
        let conn = rusqlite::Connection::open(path)
            .map_err(|e| LrgpError::Store(format!("open error: {e}")))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| LrgpError::Store(format!("pragma error: {e}")))?;

        let store = Self {
            conn: Mutex::new(conn),
        };
        store.init_tables()?;
        Ok(store)
    }

    /// Open an in-memory store (mainly for testing).
    pub fn open_memory() -> Result<Self, LrgpError> {
        let conn = rusqlite::Connection::open_in_memory()
            .map_err(|e| LrgpError::Store(format!("open_in_memory error: {e}")))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| LrgpError::Store(format!("pragma error: {e}")))?;

        let store = Self {
            conn: Mutex::new(conn),
        };
        store.init_tables()?;
        Ok(store)
    }

    fn init_tables(&self) -> Result<(), LrgpError> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS game_sessions (
                session_id   TEXT NOT NULL,
                identity_id  TEXT NOT NULL,
                app_id       TEXT NOT NULL,
                app_version  INTEGER NOT NULL DEFAULT 1,
                contact_hash TEXT NOT NULL DEFAULT '',
                initiator    TEXT NOT NULL DEFAULT '',
                status       TEXT NOT NULL DEFAULT 'pending',
                metadata     TEXT NOT NULL DEFAULT '{}',
                unread       INTEGER NOT NULL DEFAULT 0,
                created_at   REAL NOT NULL,
                updated_at   REAL NOT NULL,
                last_action_at REAL NOT NULL,
                PRIMARY KEY (session_id, identity_id)
            );

            CREATE TABLE IF NOT EXISTS game_actions (
                session_id   TEXT NOT NULL,
                identity_id  TEXT NOT NULL,
                action_num   INTEGER NOT NULL,
                command      TEXT NOT NULL,
                payload_json TEXT NOT NULL DEFAULT '{}',
                sender       TEXT NOT NULL DEFAULT '',
                timestamp    REAL NOT NULL,
                UNIQUE(session_id, identity_id, action_num)
            );
            ",
        )
        .map_err(|e| LrgpError::Store(format!("init_tables error: {e}")))?;
        Ok(())
    }

    // ──── Sessions ────

    /// Save a new session.
    #[allow(clippy::too_many_arguments)] // SQL row constructor — every column is load-bearing.
    pub fn save_session(
        &self,
        session_id: &str,
        identity_id: &str,
        app_id: &str,
        app_version: u32,
        contact_hash: &str,
        initiator: &str,
        status: &str,
        metadata: &HashMap<String, JsonValue>,
        unread: i64,
        created_at: f64,
        updated_at: f64,
        last_action_at: f64,
    ) -> Result<(), LrgpError> {
        let conn = self.conn.lock().unwrap();
        let meta_json = serde_json::to_string(metadata)
            .map_err(|e| LrgpError::Store(format!("metadata serialization error: {e}")))?;

        conn.execute(
            "INSERT OR REPLACE INTO game_sessions
             (session_id, identity_id, app_id, app_version, contact_hash, initiator,
              status, metadata, unread, created_at, updated_at, last_action_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                session_id,
                identity_id,
                app_id,
                app_version,
                contact_hash,
                initiator,
                status,
                meta_json,
                unread,
                created_at,
                updated_at,
                last_action_at,
            ],
        )
        .map_err(|e| LrgpError::Store(format!("save_session error: {e}")))?;

        Ok(())
    }

    /// Update specific columns of a session (allowlist-validated).
    pub fn update_session(
        &self,
        session_id: &str,
        identity_id: &str,
        updates: &HashMap<String, String>,
    ) -> Result<(), LrgpError> {
        if updates.is_empty() {
            return Ok(());
        }

        // Validate all keys against allowlist
        for key in updates.keys() {
            if !ALLOWED_COLUMNS.contains(&key.as_str()) {
                return Err(LrgpError::Store(format!("invalid column: {key}")));
            }
        }

        let conn = self.conn.lock().unwrap();

        let set_clause: Vec<String> = updates
            .keys()
            .enumerate()
            .map(|(i, k)| format!("{k} = ?{}", i + 3))
            .collect();
        let sql = format!(
            "UPDATE game_sessions SET {} WHERE session_id = ?1 AND identity_id = ?2",
            set_clause.join(", ")
        );

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        params.push(Box::new(session_id.to_string()));
        params.push(Box::new(identity_id.to_string()));
        for key in updates.keys() {
            params.push(Box::new(updates[key].clone()));
        }

        let params_ref: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        conn.execute(&sql, params_ref.as_slice())
            .map_err(|e| LrgpError::Store(format!("update_session error: {e}")))?;

        Ok(())
    }

    /// Retrieve a session by primary key.
    pub fn get_session(
        &self,
        session_id: &str,
        identity_id: &str,
    ) -> Result<Option<crate::session::Session>, LrgpError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT session_id, identity_id, app_id, app_version, contact_hash,
                        initiator, status, metadata, unread, created_at, updated_at,
                        last_action_at
                 FROM game_sessions WHERE session_id = ?1 AND identity_id = ?2",
            )
            .map_err(|e| LrgpError::Store(format!("get_session prepare error: {e}")))?;

        let result = stmt
            .query_row(rusqlite::params![session_id, identity_id], |row| {
                Ok(session_from_row(row))
            })
            .optional()
            .map_err(|e| LrgpError::Store(format!("get_session query error: {e}")))?;

        Ok(result)
    }

    /// List sessions, optionally filtered by status and/or identity.
    pub fn list_sessions(
        &self,
        identity_id: Option<&str>,
        status: Option<&str>,
        app_id: Option<&str>,
    ) -> Result<Vec<crate::session::Session>, LrgpError> {
        let conn = self.conn.lock().unwrap();
        let mut conditions = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(id) = identity_id {
            params.push(Box::new(id.to_string()));
            conditions.push(format!("identity_id = ?{}", params.len()));
        }
        if let Some(st) = status {
            params.push(Box::new(st.to_string()));
            conditions.push(format!("status = ?{}", params.len()));
        }
        if let Some(ai) = app_id {
            params.push(Box::new(ai.to_string()));
            conditions.push(format!("app_id = ?{}", params.len()));
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };

        let sql = format!(
            "SELECT session_id, identity_id, app_id, app_version, contact_hash,
                    initiator, status, metadata, unread, created_at, updated_at,
                    last_action_at
             FROM game_sessions{} ORDER BY updated_at DESC",
            where_clause
        );

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| LrgpError::Store(format!("list_sessions prepare error: {e}")))?;

        let params_ref: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt
            .query_map(params_ref.as_slice(), |row| Ok(session_from_row(row)))
            .map_err(|e| LrgpError::Store(format!("list_sessions query error: {e}")))?;

        let mut sessions = Vec::new();
        for row in rows {
            sessions
                .push(row.map_err(|e| LrgpError::Store(format!("list_sessions row error: {e}")))?);
        }
        Ok(sessions)
    }

    /// Delete a session and its actions.
    pub fn delete_session(&self, session_id: &str, identity_id: &str) -> Result<(), LrgpError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM game_actions WHERE session_id = ?1 AND identity_id = ?2",
            rusqlite::params![session_id, identity_id],
        )
        .map_err(|e| LrgpError::Store(format!("delete actions error: {e}")))?;

        conn.execute(
            "DELETE FROM game_sessions WHERE session_id = ?1 AND identity_id = ?2",
            rusqlite::params![session_id, identity_id],
        )
        .map_err(|e| LrgpError::Store(format!("delete session error: {e}")))?;

        Ok(())
    }

    // ──── Actions ────

    /// Save a game action.
    pub fn save_action(&self, action: &Action) -> Result<(), LrgpError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO game_actions
             (session_id, identity_id, action_num, command, payload_json, sender, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                action.session_id,
                action.identity_id,
                action.action_num,
                action.command,
                action.payload_json,
                action.sender,
                action.timestamp,
            ],
        )
        .map_err(|e| LrgpError::Store(format!("save_action error: {e}")))?;
        Ok(())
    }

    /// List all actions for a session, ordered by action_num.
    pub fn list_actions(
        &self,
        session_id: &str,
        identity_id: &str,
    ) -> Result<Vec<Action>, LrgpError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT session_id, identity_id, action_num, command,
                        payload_json, sender, timestamp
                 FROM game_actions
                 WHERE session_id = ?1 AND identity_id = ?2
                 ORDER BY action_num ASC",
            )
            .map_err(|e| LrgpError::Store(format!("list_actions prepare error: {e}")))?;

        let rows = stmt
            .query_map(rusqlite::params![session_id, identity_id], |row| {
                Ok(Action {
                    session_id: row.get(0)?,
                    identity_id: row.get(1)?,
                    action_num: row.get(2)?,
                    command: row.get(3)?,
                    payload_json: row.get(4)?,
                    sender: row.get(5)?,
                    timestamp: row.get(6)?,
                })
            })
            .map_err(|e| LrgpError::Store(format!("list_actions query error: {e}")))?;

        let mut actions = Vec::new();
        for row in rows {
            actions
                .push(row.map_err(|e| LrgpError::Store(format!("list_actions row error: {e}")))?);
        }
        Ok(actions)
    }

    /// Get the next action number for a session.
    pub fn next_action_num(&self, session_id: &str, identity_id: &str) -> Result<i64, LrgpError> {
        let conn = self.conn.lock().unwrap();
        let max: Option<i64> = conn
            .query_row(
                "SELECT MAX(action_num) FROM game_actions
                 WHERE session_id = ?1 AND identity_id = ?2",
                rusqlite::params![session_id, identity_id],
                |row| row.get(0),
            )
            .map_err(|e| LrgpError::Store(format!("next_action_num error: {e}")))?;

        Ok(max.unwrap_or(0) + 1)
    }

    /// Delete all actions for a session.
    pub fn delete_actions(&self, session_id: &str, identity_id: &str) -> Result<(), LrgpError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM game_actions WHERE session_id = ?1 AND identity_id = ?2",
            rusqlite::params![session_id, identity_id],
        )
        .map_err(|e| LrgpError::Store(format!("delete_actions error: {e}")))?;
        Ok(())
    }
}

use rusqlite::OptionalExtension;

fn session_from_row(row: &rusqlite::Row) -> crate::session::Session {
    let metadata_str: String = row.get::<_, String>(7).unwrap_or_else(|_| "{}".into());
    let metadata: HashMap<String, JsonValue> =
        serde_json::from_str(&metadata_str).unwrap_or_default();

    crate::session::Session {
        session_id: row.get(0).unwrap_or_default(),
        identity_id: row.get(1).unwrap_or_default(),
        app_id: row.get(2).unwrap_or_default(),
        app_version: row.get::<_, u32>(3).unwrap_or(1),
        contact_hash: row.get(4).unwrap_or_default(),
        initiator: row.get(5).unwrap_or_default(),
        status: row.get(6).unwrap_or_default(),
        metadata,
        unread: row.get(8).unwrap_or(0),
        created_at: row.get(9).unwrap_or(0.0),
        updated_at: row.get(10).unwrap_or(0.0),
        last_action_at: row.get(11).unwrap_or(0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> LrgpStore {
        LrgpStore::open_memory().unwrap()
    }

    #[test]
    fn test_save_and_get_session() {
        let store = test_store();
        let mut meta = HashMap::new();
        meta.insert("board".into(), JsonValue::String("_________".into()));

        store
            .save_session(
                "s1", "id1", "ttt", 1, "remote", "id1", "pending", &meta, 0, 1.0, 1.0, 1.0,
            )
            .unwrap();

        let session = store.get_session("s1", "id1").unwrap().unwrap();
        assert_eq!(session.session_id, "s1");
        assert_eq!(session.app_id, "ttt");
        assert_eq!(session.status, "pending");
        assert_eq!(
            session.metadata.get("board").unwrap().as_str().unwrap(),
            "_________"
        );
    }

    #[test]
    fn test_update_session() {
        let store = test_store();
        store
            .save_session(
                "s1",
                "id1",
                "ttt",
                1,
                "remote",
                "id1",
                "pending",
                &HashMap::new(),
                0,
                1.0,
                1.0,
                1.0,
            )
            .unwrap();

        let mut updates = HashMap::new();
        updates.insert("status".into(), "active".into());
        updates.insert("unread".into(), "1".into());
        store.update_session("s1", "id1", &updates).unwrap();

        let session = store.get_session("s1", "id1").unwrap().unwrap();
        assert_eq!(session.status, "active");
    }

    #[test]
    fn test_update_session_rejects_invalid_column() {
        let store = test_store();
        store
            .save_session(
                "s1",
                "id1",
                "ttt",
                1,
                "remote",
                "id1",
                "pending",
                &HashMap::new(),
                0,
                1.0,
                1.0,
                1.0,
            )
            .unwrap();

        let mut updates = HashMap::new();
        updates.insert("evil_column; DROP TABLE--".into(), "hack".into());
        let result = store.update_session("s1", "id1", &updates);
        assert!(result.is_err());
    }

    #[test]
    fn test_list_sessions() {
        let store = test_store();
        for i in 0..3 {
            store
                .save_session(
                    &format!("s{i}"),
                    "id1",
                    "ttt",
                    1,
                    "remote",
                    "id1",
                    if i == 2 { "active" } else { "pending" },
                    &HashMap::new(),
                    0,
                    1.0,
                    1.0,
                    1.0,
                )
                .unwrap();
        }

        let all = store.list_sessions(Some("id1"), None, None).unwrap();
        assert_eq!(all.len(), 3);

        let pending = store
            .list_sessions(Some("id1"), Some("pending"), None)
            .unwrap();
        assert_eq!(pending.len(), 2);
    }

    #[test]
    fn test_delete_session() {
        let store = test_store();
        store
            .save_session(
                "s1",
                "id1",
                "ttt",
                1,
                "remote",
                "id1",
                "pending",
                &HashMap::new(),
                0,
                1.0,
                1.0,
                1.0,
            )
            .unwrap();
        store.delete_session("s1", "id1").unwrap();
        assert!(store.get_session("s1", "id1").unwrap().is_none());
    }

    #[test]
    fn test_save_and_list_actions() {
        let store = test_store();
        for i in 1..=3 {
            store
                .save_action(&Action {
                    session_id: "s1".into(),
                    identity_id: "id1".into(),
                    action_num: i,
                    command: "move".into(),
                    payload_json: format!("{{\"n\":{i}}}"),
                    sender: "player1".into(),
                    timestamp: i as f64,
                })
                .unwrap();
        }

        let actions = store.list_actions("s1", "id1").unwrap();
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].action_num, 1);
        assert_eq!(actions[2].action_num, 3);
    }

    #[test]
    fn test_next_action_num() {
        let store = test_store();
        assert_eq!(store.next_action_num("s1", "id1").unwrap(), 1);

        store
            .save_action(&Action {
                session_id: "s1".into(),
                identity_id: "id1".into(),
                action_num: 1,
                command: "move".into(),
                payload_json: "{}".into(),
                sender: "p1".into(),
                timestamp: 1.0,
            })
            .unwrap();
        assert_eq!(store.next_action_num("s1", "id1").unwrap(), 2);
    }
}
