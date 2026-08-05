/// LRGP error hierarchy.
#[derive(Debug, thiserror::Error)]
pub enum LrgpError {
    #[error("envelope too large: {0} bytes (max {1})")]
    EnvelopeTooLarge(usize, usize),

    #[error("invalid envelope: {0}")]
    InvalidEnvelope(String),

    #[error("illegal transition: cannot apply '{command}' to session in '{status}' state")]
    IllegalTransition { command: String, status: String },

    #[error("unknown game: {0}")]
    UnknownApp(String),

    #[error("unsupported {app_id} protocol version {received}; supported version is {supported}")]
    UnsupportedVersion {
        app_id: String,
        received: u32,
        supported: u32,
    },

    #[error("unsupported action '{command}' for game '{app_id}'")]
    UnsupportedAction { app_id: String, command: String },

    #[error("peer is not authorized for session {session_id}")]
    UnauthorizedPeer { session_id: String },

    #[error("session expired: {0}")]
    SessionExpired(String),

    #[error("session not found: {0}")]
    SessionNotFound(String),

    #[error("session already exists: {0}")]
    SessionExists(String),

    #[error("a remote peer is required when creating a challenge")]
    ParticipantRequired,

    #[error("incoming dispatch requires a non-empty transport-authenticated sender")]
    AuthenticatedSenderRequired,

    #[error("incoming dispatch requires a non-empty receiving local identity")]
    ReceivingIdentityRequired,

    #[error("outgoing dispatch requires a non-empty local identity")]
    OutgoingIdentityRequired,

    #[error("incoming challenge admission limit reached ({scope}: {limit})")]
    AdmissionLimit { scope: &'static str, limit: usize },

    #[error("validation error [{code}]: {message}")]
    Validation { code: String, message: String },

    #[error("store error: {0}")]
    Store(String),
}
