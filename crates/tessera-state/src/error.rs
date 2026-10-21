//! Error and diagnostic types (ADR 0009: per-crate error enums).

use std::fmt;
use std::path::PathBuf;

/// How serious an [`Issue`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Worth reporting, does not stop anything.
    Warning,
    /// The configuration or state cannot be used as is.
    Error,
}

/// One finding from loading or validating configuration: where, and what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Error or warning.
    pub severity: Severity,
    /// Key path, for example `device[0].sample_rate` or
    /// `device[1].rx_channel_labels.5`. Empty for whole-file problems such as
    /// a syntax error.
    pub path: String,
    /// What is wrong.
    pub message: String,
    /// The file that set the offending key (from provenance), if known.
    pub source: Option<PathBuf>,
}

impl Issue {
    pub(crate) fn error(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            path: path.into(),
            message: message.into(),
            source: None,
        }
    }

    pub(crate) fn warning(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            path: path.into(),
            message: message.into(),
            source: None,
        }
    }

    /// True for [`Severity::Error`].
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sev = match self.severity {
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        write!(f, "{sev}: ")?;
        if !self.path.is_empty() {
            write!(f, "{}: ", self.path)?;
        }
        f.write_str(&self.message)?;
        if let Some(src) = &self.source {
            write!(f, " (in {})", src.display())?;
        }
        Ok(())
    }
}

/// Failure to load configuration.
#[derive(Debug)]
#[non_exhaustive]
pub enum ConfigError {
    /// A file could not be read.
    Io {
        /// The file or directory.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The configuration is not valid. Contains every error found (and the
    /// warnings), not just the first.
    Invalid {
        /// All findings, errors first in discovery order.
        issues: Vec<Issue>,
    },
}

impl ConfigError {
    /// Only the errors among the findings.
    pub fn errors(&self) -> impl Iterator<Item = &Issue> {
        let slice: &[Issue] = match self {
            Self::Invalid { issues } => issues,
            Self::Io { .. } => &[],
        };
        slice.iter().filter(|i| i.is_error())
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            Self::Invalid { issues } => {
                let n = issues.iter().filter(|i| i.is_error()).count();
                write!(f, "configuration has {n} error(s)")?;
                for i in issues {
                    write!(f, "\n  {i}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Invalid { .. } => None,
        }
    }
}

/// Failure to read or write the state file.
#[derive(Debug)]
#[non_exhaustive]
pub enum StateError {
    /// An I/O operation failed.
    Io {
        /// What was being done.
        action: &'static str,
        /// The file or directory.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The state could not be serialised (a bug).
    Serialize(String),
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                action,
                path,
                source,
            } => write!(f, "{action} {}: {source}", path.display()),
            Self::Serialize(m) => write!(f, "cannot serialise state: {m}"),
        }
    }
}

impl std::error::Error for StateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Serialize(_) => None,
        }
    }
}

/// Failure to start the state engine.
#[derive(Debug)]
#[non_exhaustive]
pub enum StartError {
    /// The configuration is unusable.
    Config(ConfigError),
    /// The configuration is valid but cannot be used on this host (an
    /// interface does not exist or is wireless without permission).
    Host(Vec<Issue>),
    /// The state file exists but could not be read or backed up. The engine
    /// refuses to start rather than overwrite it.
    State(StateError),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(e) => e.fmt(f),
            Self::Host(issues) => {
                f.write_str("configuration cannot be used on this host")?;
                for i in issues {
                    write!(f, "\n  {i}")?;
                }
                Ok(())
            }
            Self::State(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Config(e) => Some(e),
            Self::State(e) => Some(e),
            Self::Host(_) => None,
        }
    }
}
