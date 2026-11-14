mod clipboard;
mod config;
mod history;
mod source;

use anyhow::{Context, Result, bail, ensure};
use chrono::{SecondsFormat, Utc};
use orbment_core::i18n::{self, tr};
use rusqlite::{Connection, backup::Backup};
use serde::Serialize;
use source::{Input, Kind};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND},
        System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE},
    },
    core::w,
};

#[derive(Default)]
struct Options {
    dry_run: bool,
    only_unset: bool,
    source: Option<Kind>,
    custom: Vec<Input>,
    source_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    help: bool,
}
impl Options {
    fn parse() -> Result<Self> {
        let mut result = Self::default();
        let mut args = std::env::args_os().skip(1);
        while let Some(argument) = args.next() {
            let mut value = || args.next().context(tr("importer.invalid_arguments"));
            match argument.to_str() {
                Some("--help" | "-h") => result.help = true,
                Some("--dry-run") => result.dry_run = true,
                Some("--only-unset-config") => result.only_unset = true,
                Some("--data-dir") => result.data_dir = Some(PathBuf::from(value()?)),
                Some("--source-dir") => result.source_dir = Some(PathBuf::from(value()?)),
                Some("--source") => {
                    let name = value()?;
                    let name = name.to_str().context(tr("importer.invalid_arguments"))?;
                    result.source = if name == "all" {
                        None
                    } else {
                        Some(Kind::parse(name)?)
                    };
                }
                Some("--source-path") => {
                    let raw = value()?;
                    let raw = raw.to_str().context(tr("importer.invalid_arguments"))?;
                    let (name, path) = raw
                        .split_once('=')
                        .context(tr("importer.invalid_arguments"))?;
                    ensure!(!path.is_empty(), "{}", tr("importer.invalid_arguments"));
                    result.custom.push(Input {
                        kind: Kind::parse(name)?,
                        path: PathBuf::from(path),
                    });
                }
                Some("--clipboard" | "--audio" | "--monitor" | "--vision" | "--everything") => {
                    let name = argument
                        .to_str()
                        .context(tr("importer.invalid_arguments"))?
                        .trim_start_matches('-');
                    result.custom.push(Input {
                        kind: Kind::parse(name)?,
                        path: PathBuf::from(value()?),
                    });
                }
                _ => bail!(
                    "{}: {}",
                    tr("importer.invalid_arguments"),
                    argument.to_string_lossy()
                ),
            }
        }
        if let Some(path) = result.source_dir.take() {
            result.custom.push(Input {
                kind: result
                    .source
                    .context(tr("importer.source_dir_requires_source"))?,
                path,
            });
        }
        Ok(result)
    }
}

fn require_stopped() -> Result<()> {
    unsafe {
        match OpenMutexW(
            SYNCHRONIZATION_SYNCHRONIZE,
            false,
            w!("Local\\Orbment_SingleInstance"),
        ) {
            Ok(handle) => {
                let _ = CloseHandle(handle);
                bail!("{}", tr("importer.main_running"));
            }
            Err(error) if error.code() == ERROR_ACCESS_DENIED.to_hresult() => {
                bail!("{}", tr("importer.main_running"))
            }
            Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => Ok(()),
            Err(error) => Err(error).context(tr("importer.mutex_failed")),
        }
    }
}

#[derive(Serialize)]
struct Report {
    dry_run: bool,
    target: PathBuf,
    compared_with_existing_database: bool,
    sources: Vec<Input>,
    source_files: Vec<PathBuf>,
    source_counts: BTreeMap<String, usize>,
    clipboard: clipboard::ClipboardReport,
    traffic_source_rows: usize,
    traffic_unique_days: usize,
    everything_rows: usize,
    configuration: config::ConfigReport,
    warnings: Vec<String>,
}

fn run(options: Options) -> Result<Report> {
    require_stopped()?;
    let executable = std::env::current_exe()?;
    let exe_dir = executable
        .parent()
        .context(tr("importer.invalid_arguments"))?;
    let env_dir = std::env::var_os("ORBMENT_DATA_DIR").map(PathBuf::from);
    let appdata = std::env::var_os("APPDATA").map(PathBuf::from);
    let target = orbment_core::config::resolve_data_dir(
        exe_dir,
        options.data_dir.as_deref(),
        env_dir.as_deref(),
        appdata.as_deref(),
    )?;
    i18n::load(&exe_dir.join("assets/locales/zh-CN.toml"))?;
    let temporary = tempfile::tempdir()?;
    let sources = source::discover(options.source, &options.custom)?;
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    let mut warnings = Vec::new();
    for input in &sources {
        let found = source::files(input)?;
        if found.is_empty() {
            warnings.push(format!(
                "{}: {}",
                tr("importer.source_missing"),
                input.path.display()
            ));
        }
        for file in found {
            let file = fs::canonicalize(file)?;
            for name in [
                orbment_core::db::FILE_NAME,
                "orbment.db-wal",
                "orbment.db-shm",
                "config.toml",
            ] {
                let destination = target.join(name);
                if destination.exists() {
                    ensure!(
                        !same_file::is_same_file(&file, &destination)?,
                        "{}: {}",
                        tr("importer.source_is_target"),
                        file.display()
                    );
                }
            }
            if seen.insert((input.kind, file.clone())) {
                files.push((input.kind, file));
            }
        }
    }
    ensure!(!files.is_empty(), "{}", tr("importer.source_missing"));
    // Modern clipboard history wins over legacy history. Configuration order is the reverse.
    files.sort_by_key(|(kind, path)| {
        (
            *kind,
            usize::from(path.file_name().is_some_and(|v| v == "legacy_clipboard.db")),
            path.clone(),
        )
    });
    let mut working = Connection::open(temporary.path().join("working.db"))?;
    let existing = target.join(orbment_core::db::FILE_NAME);
    let seeded = existing.is_file();
    if seeded {
        let snapshot = source::snapshot(&existing, temporary.path(), 0)?;
        Backup::new(&snapshot, &mut working)?.run_to_completion(
            256,
            Duration::from_millis(20),
            None,
        )?;
    }
    orbment_core::db::initialize(&mut working)?;
    let mut config = config::ConfigPlan::new(
        &target,
        &temporary.path().join("configuration"),
        options.only_unset,
    )?;
    let mut media = clipboard::Media::new(temporary.path().join("media"), target.clone())?;
    let mut report = Report {
        dry_run: options.dry_run,
        target: target.clone(),
        compared_with_existing_database: seeded,
        sources,
        source_files: files.iter().map(|(_, p)| p.clone()).collect(),
        clipboard: Default::default(),
        source_counts: BTreeMap::new(),
        traffic_source_rows: 0,
        traffic_unique_days: 0,
        everything_rows: 0,
        configuration: Default::default(),
        warnings,
    };
    let mut traffic = BTreeMap::new();
    let mut configs = Vec::new();
    let tx = working.transaction()?;
    for (index, (kind, path)) in files.iter().enumerate() {
        let name = path
            .file_name()
            .and_then(|v| v.to_str())
            .context(tr("importer.invalid_source"))?;
        match (kind, name) {
            (Kind::Clipboard, "clipboard.db" | "legacy_clipboard.db") => {
                let before = report.clipboard.source_items;
                let snapshot = source::snapshot(path, temporary.path(), index + 1)?;
                clipboard::import(
                    &tx,
                    &snapshot,
                    path,
                    name == "clipboard.db",
                    &mut media,
                    &mut report.clipboard,
                )
                .with_context(|| {
                    format!("{}: {}", tr("importer.invalid_source"), path.display())
                })?;
                report.source_counts.insert(
                    path.display().to_string(),
                    report.clipboard.source_items - before,
                );
            }
            (
                Kind::Monitor,
                "history_traffic.dat" | "history_traffic.dat.bak" | "history_traffic.bak",
            ) => {
                let bytes = source::read_bytes(path, 128 * 1024 * 1024)?;
                if bytes.iter().all(|v| *v == 0) {
                    report.warnings.push(format!(
                        "{}: {}",
                        tr("importer.empty_history"),
                        path.display()
                    ));
                    report.source_counts.insert(path.display().to_string(), 0);
                    continue;
                }
                let text = source::decode_text(&bytes)?;
                let days = history::parse_traffic(&text).with_context(|| {
                    format!("{}: {}", tr("importer.invalid_source"), path.display())
                })?;
                let count = text
                    .lines()
                    .filter(|v| !v.trim().is_empty())
                    .count()
                    .saturating_sub(1);
                report.traffic_source_rows += count;
                report
                    .source_counts
                    .insert(path.display().to_string(), count);
                history::merge_traffic(&mut traffic, days);
            }
            (Kind::Everything, _) => {
                let count =
                    history::import_everything(&tx, &source::read_bytes(path, 256 * 1024 * 1024)?)?;
                report.everything_rows += count;
                report
                    .source_counts
                    .insert(path.display().to_string(), count);
            }
            (Kind::Clipboard, "settings.json" | ".store.json")
            | (Kind::Audio, "legacy_audio.json")
            | (Kind::Monitor, "config.ini" | "legacy_monitor.ini")
            | (Kind::Vision, "legacy_vision.json" | "settings.json") => {
                configs.push((*kind, path.clone()))
            }
            _ => report.warnings.push(format!(
                "{}: {}",
                tr("importer.unsupported_source"),
                path.display()
            )),
        }
    }
    report.clipboard.skipped_items = report.clipboard.source_items
        - report.clipboard.inserted_items
        - report.clipboard.duplicate_items;
    report.traffic_unique_days = traffic.len();
    history::write_traffic(
        &tx,
        &traffic,
        &Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
    )?;
    tx.commit()?;
    configs.sort_by_key(|(kind, path)| {
        (
            *kind,
            usize::from(path.file_name().is_none_or(|v| v != ".store.json")),
            path.clone(),
        )
    });
    for (kind, path) in configs {
        config.import(&path, kind)?;
    }
    if !options.dry_run {
        require_stopped()?;
        fs::create_dir_all(&target)?;
        media.publish()?;
        let mut destination = orbment_core::db::open(&target)?;
        Backup::new(&working, &mut destination)?
            .run_to_completion(256, Duration::from_millis(20), None)
            .context(tr("importer.write_failed"))?;
        config
            .publish()
            .context(tr("importer.config_after_database_failed"))?;
        orbment_core::db::checkpoint_truncate(&destination)?;
    }
    report.configuration = config.report;
    Ok(report)
}

fn main() {
    let result = (|| -> Result<()> {
        let options = Options::parse()?;
        if options.help {
            println!("{}", tr("importer.help"));
            return Ok(());
        }
        println!("{}", serde_json::to_string_pretty(&run(options)?)?);
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("{}: {error:#}", tr("importer.failed"));
        std::process::exit(1);
    }
}
