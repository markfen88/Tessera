//! `tracing` setup for the control threads (ADR 0009). RT threads never log.

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Registry, fmt, reload};

/// Lets the log filter change at run time (SIGHUP).
#[derive(Debug)]
pub struct LogControl {
    handle: Option<reload::Handle<EnvFilter, Registry>>,
    env_wins: bool,
}

/// Install the global subscriber writing to stderr (journald captures it).
///
/// `RUST_LOG`, when set, overrides every other source, now and on reload.
/// Calling this twice is harmless (the second call keeps the first
/// subscriber), which matters for tests.
#[must_use]
pub fn init(initial: &str) -> LogControl {
    let env_wins = std::env::var_os("RUST_LOG").is_some();
    let filter = if env_wins {
        EnvFilter::from_default_env()
    } else {
        EnvFilter::try_new(initial).unwrap_or_else(|_| EnvFilter::new("info"))
    };
    let (filter, handle) = reload::Layer::new(filter);
    let under_journald = std::env::var_os("JOURNAL_STREAM").is_some();
    let layer = fmt::layer().with_writer(std::io::stderr).with_ansi(false);
    let ok = if under_journald {
        tracing_subscriber::registry()
            .with(filter)
            .with(layer.without_time())
            .try_init()
            .is_ok()
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(layer)
            .try_init()
            .is_ok()
    };
    LogControl {
        handle: ok.then_some(handle),
        env_wins,
    }
}

impl LogControl {
    /// Is `RUST_LOG` set (it then wins over `[daemon] log_level` and
    /// `--log-level`, now and on reload)?
    #[must_use]
    pub fn env_wins(&self) -> bool {
        self.env_wins
    }

    /// Apply a new filter specification. A no-op when `RUST_LOG` is set.
    ///
    /// # Errors
    /// The specification does not parse (the old filter stays).
    pub fn set_filter(&self, spec: &str) -> Result<(), String> {
        if self.env_wins {
            return Ok(());
        }
        let Some(h) = &self.handle else {
            return Ok(());
        };
        let f = EnvFilter::try_new(spec).map_err(|e| e.to_string())?;
        h.reload(f).map_err(|e| e.to_string())
    }
}
