//! Error taxonomy for mebookmarker.
//!
//! Every fallible operation in the workspace returns [`Error`]. The variants
//! are grouped by *what the caller can do about them*, not by where they
//! happen, so a caller can write one `match` that covers a whole pipeline
//! stage without knowing which crate produced the failure.

use std::path::PathBuf;

/// The result type used throughout the workspace.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Where a failure came from, used to make errors actionable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Layer {
    /// Configuration parsing or validation.
    Config,
    /// Storage engine.
    Store,
    /// A source adapter pulling bookmarks in.
    Ingest,
    /// Content extraction for a single link.
    Extract,
    /// The JEV evaluation client.
    Jev,
    /// A local coding-agent driver.
    Agent,
    /// A sink writing bookmarks out.
    Sink,
    /// The pipeline itself.
    Pipeline,
    /// The command-line or TUI front end.
    Ui,
}

impl Layer {
    /// Short, stable label used in log lines and error prefixes.
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

/// A classification of [`Error`] that drives retry and skip behaviour.
///
/// The pipeline consults this to decide whether an item should be retried
/// later, retried immediately, or parked permanently. Getting this wrong in
/// either direction is expensive: retrying a permanent failure forever stalls
/// the run, while parking a transient one silently drops data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    /// The failure is about the input and will fail identically forever.
    Permanent,
    /// The failure is about the environment and may resolve on its own.
    Transient,
    /// Authentication or authorisation failed; retrying without new
    /// credentials is pointless.
    Auth,
    /// The request was rejected as abusive. Back off, then retry.
    RateLimited,
    /// The remote end does not know what we asked for.
    NotFound,
}

impl Class {
    /// Whether a pipeline should try this operation again later.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Transient | Self::RateLimited)
    }
}

/// The single error type of the workspace.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Configuration was missing, malformed, or self-contradictory.
    #[error("config: {0}")]
    Config(String),

    /// A source adapter could not produce bookmarks.
    #[error("ingest: {0}")]
    Ingest(String),

    /// Content extraction failed for a specific URL.
    #[error("extract: {url}: {reason}")]
    Extract {
        /// The URL whose content could not be extracted.
        url: String,
        /// What went wrong.
        reason: String,
    },

    /// The JEV evaluation endpoint returned an error.
    #[error("jev: {0}")]
    Jev(String),

    /// A local coding-agent driver failed.
    #[error("agent: {0}")]
    Agent(String),

    /// A sink could not write its output.
    #[error("sink: {0}")]
    Sink(String),

    /// The storage engine failed.
    #[error("store: {0}")]
    Store(String),

    /// The pipeline could not make progress.
    #[error("pipeline: {0}")]
    Pipeline(String),

    /// A filesystem operation failed.
    #[error("io: {path}: {source}")]
    Io {
        /// The path being operated on.
        path: PathBuf,
        /// The underlying I/O failure.
        #[source]
        source: std::io::Error,
    },

    /// An external process exited non-zero.
    #[error("command `{program}` failed with {status}: {stderr}")]
    Command {
        /// The program that was run.
        program: String,
        /// How it exited.
        status: String,
        /// Its last output on stderr.
        stderr: String,
    },

    /// An operation exceeded its deadline.
    #[error("timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// An HTTP request failed.
    #[error("http: {0}")]
    Http(String),

    /// Credentials are missing, expired, or rejected.
    #[error("authentication: {0}")]
    Auth(String),

    /// The remote rate limit was hit.
    #[error("rate limited: {0}")]
    RateLimited(String),

    /// The requested item does not exist.
    #[error("not found: {0}")]
    NotFound(String),

    /// The input was structurally valid but semantically wrong.
    #[error("invalid input: {0}")]
    Invalid(String),

    /// Something else went wrong.
    #[error("internal: {0}")]
    Internal(String),
}

impl Error {
    /// Build a [`Class::Permanent`] error with no extra context.
    pub fn invalid(msg: impl Into<String>) -> Self {
        Self::Invalid(msg.into())
    }

    /// Build a [`Class::Permanent`] pipeline error with no extra context.
    pub fn pipeline(msg: impl Into<String>) -> Self {
        Self::Pipeline(msg.into())
    }

    /// Build a [`Class::Permanent`] internal error with no extra context.
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }

    /// Build a [`Class::Permanent`] extraction error.
    pub fn extract(url: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::Extract { url: url.into(), reason: reason.into() }
    }

    /// How a caller should react to this failure.
    #[must_use]
    pub const fn class(&self) -> Class {
        match self {
            // Configuration and structural problems never fix themselves.
            Self::Config(_)
            | Self::Invalid(_)
            | Self::Extract { .. }
            | Self::Internal(_)
            | Self::Pipeline(_) => Class::Permanent,
            // Anything raised by the pipeline that is not explicitly classified
            // is treated as retryable, because a stage that failed for an
            // unknown reason is far more often a blip than a real bug.
            Self::Io { .. } | Self::Sink(_) | Self::Store(_) | Self::Command { .. } => {
                Class::Transient
            }
            Self::Jev(_) | Self::Ingest(_) | Self::Agent(_) | Self::Http(_) => Class::Transient,
            Self::Timeout(_) | Self::RateLimited(_) => Class::RateLimited,
            Self::Auth(_) => Class::Auth,
            Self::NotFound(_) => Class::NotFound,
        }
    }

    /// The layer that raised this error.
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
            Self::Pipeline(_) => Layer::Pipeline,
            Self::Io { .. } | Self::Http(_) | Self::Auth(_) | Self::RateLimited(_)
            | Self::Timeout(_) | Self::NotFound(_) | Self::Invalid(_) | Self::Internal(_) => {
                Layer::Pipeline
            }
        }
    }

    /// Wrap an I/O failure with the path that caused it.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permanent_failures_are_not_retryable() {
        assert!(!Error::invalid("bad").class().is_retryable());
        assert!(!Error::Config("bad".into()).class().is_retryable());
        assert!(!Error::extract("https://x.com", "boom").class().is_retryable());
    }

    #[test]
    fn transient_failures_are_retryable() {
        assert!(Error::Store("locked".into()).class().is_retryable());
        assert!(Error::Jev("500".into()).class().is_retryable());
        assert!(Error::Ingest("econnrefused".into()).class().is_retryable());
    }

    #[test]
    fn auth_and_rate_limit_are_distinguished_from_transient() {
        assert_eq!(Error::Auth("expired".into()).class(), Class::Auth);
        assert!(!Error::Auth("expired".into()).class().is_retryable());

        assert_eq!(Error::RateLimited("429".into()).class(), Class::RateLimited);
        assert!(Error::RateLimited("429".into()).class().is_retryable());
    }

    #[test]
    fn layer_matches_the_raising_crate() {
        assert_eq!(Error::Jev("x".into()).layer(), Layer::Jev);
        assert_eq!(Error::Agent("x".into()).layer(), Layer::Agent);
        assert_eq!(Error::Config("x".into()).layer(), Layer::Config);
    }

    #[test]
    fn io_errors_carry_their_path() {
        let e = Error::io("/tmp/x", std::io::Error::other("nope"));
        assert!(e.to_string().contains("/tmp/x"));
    }
}
