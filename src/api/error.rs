use std::sync::Arc;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The request never completed (DNS, TLS, timeout, connection reset…).
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),

    /// A completed request with a non-success status that isn't an auth failure.
    #[error("server returned {status}: {body}")]
    Status { status: u16, body: String },

    /// `401`/`403` from any endpoint: the session id is invalid, expired, or
    /// revoked. Callers should drop the stored session and re-authenticate.
    #[error("the session is invalid, expired, or revoked")]
    Unauthorized,

    /// The Auth0 device login expired before the user approved it.
    #[error("login expired before it was approved")]
    LoginExpired,

    /// Auth0 refused the login (bad client config, denied, …).
    #[error("login failed: {0}")]
    LoginFailed(Arc<str>),

    /// The realtime event stream connection failed.
    #[error("event stream: {0}")]
    Stream(Arc<str>),

    /// Anything else worth naming.
    #[error("{0}")]
    Other(Arc<str>),
}

impl Error {
    /// `true` for [`Error::Unauthorized`]: the one error the UI reacts to by
    /// logging out.
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Error::Unauthorized)
    }
}
