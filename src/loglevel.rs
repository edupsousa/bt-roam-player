//! Runtime log level control: SIGUSR1 makes logging more verbose, SIGUSR2 quieter.

use anyhow::Result;
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::{EnvFilter, Registry, prelude::*, reload};

type Handle = reload::Handle<EnvFilter, Registry>;

/// Quietest to most verbose.
const LEVELS: [&str; 5] = ["error", "warn", "info", "debug", "trace"];
const INFO: usize = 2;

/// Position in `LEVELS` for a plain level name such as "debug"; `None` for anything else
/// (e.g. per-module directives like "bluer=warn,info").
fn parse_level(s: &str) -> Option<usize> {
    LEVELS.iter().position(|l| l.eq_ignore_ascii_case(s.trim()))
}

/// The level reached by moving `delta` steps from `current`, clamped to the ends of `LEVELS`.
fn step(current: usize, delta: i32) -> usize {
    (current as i32 + delta).clamp(0, LEVELS.len() as i32 - 1) as usize
}

pub struct LogLevel {
    handle: Handle,
    current: usize,
}

/// Install the global subscriber. `RUST_LOG` wins over `-v` as before. The returned control
/// starts from the effective level if it is a plain one, otherwise from `info`; the first
/// signal then replaces any custom `RUST_LOG` directives with a plain level.
pub fn init(verbosity: u8) -> LogLevel {
    let from_verbosity = match verbosity {
        0 => INFO,
        1 => 3,
        _ => 4,
    };
    let env = std::env::var("RUST_LOG").ok();
    let current = env.as_deref().and_then(parse_level).unwrap_or(match env {
        Some(_) => INFO,
        None => from_verbosity,
    });
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(LEVELS[current]));
    let (filter, handle) = reload::Layer::new(filter);
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .init();
    LogLevel { handle, current }
}

impl LogLevel {
    fn change(&mut self, delta: i32) {
        let next = step(self.current, delta);
        if let Err(e) = self.handle.reload(EnvFilter::new(LEVELS[next])) {
            tracing::warn!("cannot change log level: {e}");
            return;
        }
        self.current = next;
        // Always shown, even when the level was just lowered to "error".
        tracing::error!("log level now {}", LEVELS[next]);
    }

    /// Handle SIGUSR1 (more verbose) and SIGUSR2 (quieter) until the task is dropped.
    pub fn spawn_signal_handler(mut self) -> Result<()> {
        let mut up = signal(SignalKind::user_defined1())?;
        let mut down = signal(SignalKind::user_defined2())?;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = up.recv() => self.change(1),
                    _ = down.recv() => self.change(-1),
                }
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_levels_only() {
        assert_eq!(parse_level("debug"), Some(3));
        assert_eq!(parse_level(" INFO "), Some(2));
        assert_eq!(parse_level("bluer=warn,info"), None);
        assert_eq!(parse_level(""), None);
    }

    #[test]
    fn steps_clamp_at_both_ends() {
        assert_eq!(step(INFO, 1), 3);
        assert_eq!(step(INFO, -1), 1);
        assert_eq!(step(4, 1), 4);
        assert_eq!(step(0, -1), 0);
    }
}
