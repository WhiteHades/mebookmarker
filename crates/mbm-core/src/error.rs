//! the workspace error type, grouped by what a caller can do about it.
use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Layer {
    Config,

    Store,

    Ingest,

    Extract,

    Jev,

    Agent,

    Sink,

    Pipeline,

    Ui,
}

impl Layer {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Store => "store",
            Self::Ingest => "ingest",
            Self::Extract => "extract",
            Self::Jev => "jev",
            Self::Agent => "agent",
            Self::Sink => "sink",
            Self::Pipeline => "pipeline",
            Self::Ui => "ui",
        }
    }
}

impl std::fmt::Display for Layer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
// grouped by what the caller should do about them, so one match covers a
// whole pipeline stage.
pub enum Class {
    Permanent,

    Transient,

    Auth,

    RateLimited,

    NotFound,
}

impl Class {
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Transient | Self::RateLimited)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config: {0}")]
    Config(String),

    #[error("ingest: {0}")]
    Ingest(String),

    #[error("extract: {url}: {reason}")]
    Extract { url: String, reason: String },

    #[error("jev: {0}")]
    Jev(String),

    #[error("agent: {0}")]
    Agent(String),

    #[error("sink: {0}")]
    Sink(String),

    #[error("store: {0}")]
    Store(String),

    #[error("pipeline: {0}")]
    Pipeline(String),

    #[error("io: {path}: {source}")]
    Io {
        path: PathBuf,

        #[source]
        source: std::io::Error,
    },

    #[error("command `{program}` failed with {status}: {stderr}")]
    Command { program: String, status: String, stderr: String },

    #[error("timed out after {0:?}")]
    Timeout(std::time::Duration),

    #[error("http: {0}")]
    Http(String),

    #[error("authentication: {0}")]
    Auth(String),

    #[error("rate limited: {0}")]
    RateLimited(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("invalid input: {0}")]
    Invalid(String),

    #[error("internal: {0}")]
    Internal(String),
}

impl Error {
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::Invalid(msg.into())
    }

    pub fn pipeline(msg: impl Into<String>) -> Self {
        Self::Pipeline(msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    pub fn extract(url: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Extract { url: url.into(), reason: reason.into() }
    }

    #[must_use]
    pub const fn class(&self) -> Class {
        match self {
            Self::Config(_)
            | Self::Invalid(_)
            | Self::Extract { .. }
            | Self::Internal(_)
            | Self::Pipeline(_) => Class::Permanent,

            Self::Io { .. } | Self::Sink(_) | Self::Store(_) | Self::Command { .. } => {
                Class::Transient
            }
            Self::Jev(_) | Self::Ingest(_) | Self::Agent(_) | Self::Http(_) => Class::Transient,
            Self::Timeout(_) | Self::RateLimited(_) => Class::RateLimited,
            Self::Auth(_) => Class::Auth,
            Self::NotFound(_) => Class::NotFound,
        }
    }

    #[must_use]
    pub const fn layer(&self) -> Layer {
        match self {
            Self::Config(_) => Layer::Config,
            Self::Store(_) => Layer::Store,
            Self::Ingest(_) => Layer::Ingest,
            Self::Extract { .. } => Layer::Extract,
            Self::Jev(_) => Layer::Jev,
            Self::Agent(_) | Self::Command { .. } => Layer::Agent,
            Self::Sink(_) => Layer::Sink,
            // these have no layer of their own: they are reported by a layer,
            // and the ones that are not are the caller's own mistakes
            Self::Pipeline(_)
            | Self::Io { .. }
            | Self::Http(_)
            | Self::Auth(_)
            | Self::RateLimited(_)
            | Self::Timeout(_)
            | Self::NotFound(_)
            | Self::Invalid(_)
            | Self::Internal(_) => Layer::Pipeline,
        }
    }

    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io { path: path.into(), source }
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io { path: PathBuf::from("<unknown>"), source: value }
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::Invalid(format!("json: {value}"))
    }
}

impl From<url::ParseError> for Error {
    fn from(value: url::ParseError) -> Self {
        Self::Invalid(format!("url: {value}"))
    }
}
