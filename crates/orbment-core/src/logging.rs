use std::{
    fs,
    path::Path,
    str::FromStr,
    sync::OnceLock,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result};
use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{
    Registry, filter::LevelFilter, layer::SubscriberExt, reload, util::SubscriberInitExt,
};

use crate::i18n::tr;

static FILTER: OnceLock<reload::Handle<LevelFilter, Registry>> = OnceLock::new();

/// Keep the returned guard alive until all producers have stopped logging.
pub fn init(data_dir: &Path, level: &str, retention_days: usize) -> Result<WorkerGuard> {
    let level = LevelFilter::from_str(level).with_context(|| tr("logging.invalid_level"))?;
    let directory = data_dir.join("logs");
    fs::create_dir_all(&directory).with_context(|| tr("logging.directory_failed"))?;
    cleanup_old_logs(data_dir, retention_days)?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("orbment")
        .filename_suffix("log")
        .build(&directory)
        .with_context(|| tr("logging.open_failed"))?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let (filter, handle) = reload::Layer::new(level);
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer),
        )
        .try_init()
        .with_context(|| tr("logging.initialize_failed"))?;
    let _ = FILTER.set(handle);
    Ok(guard)
}

pub fn set_level(level: &str) -> Result<()> {
    let level = LevelFilter::from_str(level).with_context(|| tr("logging.invalid_level"))?;
    FILTER
        .get()
        .with_context(|| tr("logging.not_initialized"))?
        .reload(level)
        .with_context(|| tr("logging.reload_failed"))
}

/// Apply the current retention setting at startup, on edits, and during host idle maintenance.
pub fn cleanup_old_logs(data_dir: &Path, retention_days: usize) -> Result<()> {
    let retention_seconds = u64::try_from(retention_days)?
        .checked_mul(24 * 60 * 60)
        .with_context(|| tr("logging.invalid_retention"))?;
    cleanup(
        &data_dir.join("logs"),
        Duration::from_secs(retention_seconds),
        SystemTime::now(),
    )
}

fn cleanup(directory: &Path, retention: Duration, now: SystemTime) -> Result<()> {
    for entry in fs::read_dir(directory).with_context(|| tr("logging.list_failed"))? {
        let entry = entry.with_context(|| tr("logging.list_failed"))?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        if !name.to_str().is_some_and(is_daily_log) {
            continue;
        }
        let modified = entry.metadata()?.modified()?;
        // Future timestamps after a system clock change must not be deleted.
        if now
            .duration_since(modified)
            .is_ok_and(|age| age >= retention)
        {
            fs::remove_file(entry.path()).with_context(|| {
                format!(
                    "{}: {}",
                    tr("logging.remove_failed"),
                    entry.path().display()
                )
            })?;
        }
    }
    Ok(())
}

fn is_daily_log(name: &str) -> bool {
    let Some(date) = name
        .strip_prefix("orbment.")
        .and_then(|name| name.strip_suffix(".log"))
    else {
        return false;
    };
    date.len() == 10
        && date.bytes().enumerate().all(|(index, byte)| {
            if index == 4 || index == 7 {
                byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        })
}
