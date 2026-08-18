//! Optional file logging with secret redaction.
//!
//! Logging is disabled until [`init`] is called. Secrets registered through
//! [`register_secret`] are replaced with `[redacted]` in every message.

use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{CoreError, state_file};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    #[must_use]
    pub fn parse(text: &str) -> Self {
        match text.to_ascii_lowercase().as_str() {
            "error" => Self::Error,
            "warn" | "warning" => Self::Warn,
            "debug" | "trace" => Self::Debug,
            _ => Self::Info,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
        }
    }
}

#[derive(Debug)]
struct Logger {
    file: Mutex<File>,
    level: Level,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();
static SECRETS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Open the log file (append) and enable logging at `level`.
///
/// Returns the resolved log path. Calling `init` again is a no-op.
pub fn init(path: Option<PathBuf>, level: Level) -> Result<PathBuf, CoreError> {
    let path = match path {
        Some(path) => path,
        None => state_file("terminalreader.log")?,
    };
    let file = OpenOptions::new().create(true).append(true).open(&path)?;
    let _ = LOGGER.set(Logger {
        file: Mutex::new(file),
        level,
    });
    Ok(path)
}

/// Register a secret to be redacted from all future log messages.
pub fn register_secret(secret: &str) {
    if secret.is_empty() {
        return;
    }
    if let Ok(mut secrets) = SECRETS.lock() {
        if !secrets.iter().any(|existing| existing == secret) {
            secrets.push(secret.to_owned());
        }
    }
}

fn redact(message: &str) -> String {
    let mut message = message.to_owned();
    if let Ok(secrets) = SECRETS.lock() {
        for secret in secrets.iter() {
            message = message.replace(secret, "[redacted]");
        }
    }
    message
}

/// Write a message if logging is enabled at `level`; no-op otherwise.
pub fn log(level: Level, message: &str) {
    let Some(logger) = LOGGER.get() else {
        return;
    };
    if level > logger.level {
        return;
    }
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let line = format!("{timestamp} {} {}\n", level.label(), redact(message));
    if let Ok(mut file) = logger.file.lock() {
        let _ = file.write_all(line.as_bytes());
    }
}

pub fn error(message: &str) {
    log(Level::Error, message);
}

pub fn warn(message: &str) {
    log(Level::Warn, message);
}

pub fn info(message: &str) {
    log(Level::Info, message);
}

pub fn debug(message: &str) {
    log(Level::Debug, message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_masks_registered_secrets() {
        register_secret("s3cr3t-userkey");
        let redacted = redact("auth with key s3cr3t-userkey failed");
        assert!(!redacted.contains("s3cr3t-userkey"));
        assert!(redacted.contains("[redacted]"));
    }

    #[test]
    fn level_parsing_defaults_to_info() {
        assert_eq!(Level::parse("warn"), Level::Warn);
        assert_eq!(Level::parse("DEBUG"), Level::Debug);
        assert_eq!(Level::parse("bogus"), Level::Info);
    }
}
