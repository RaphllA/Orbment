use anyhow::{Context, Result, bail, ensure};
use orbment_core::i18n::tr;
use rusqlite::{Connection, OpenFlags, backup::Backup, types::ValueRef};
use serde::Serialize;
use serde_json::{Map, Value};
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Clipboard,
    Audio,
    Monitor,
    Vision,
    Everything,
}
impl Kind {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "clipboard" => Ok(Self::Clipboard),
            "audio" => Ok(Self::Audio),
            "monitor" => Ok(Self::Monitor),
            "vision" => Ok(Self::Vision),
            "everything" => Ok(Self::Everything),
            _ => bail!("{}: {value}", tr("importer.invalid_arguments")),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Input {
    pub kind: Kind,
    pub path: PathBuf,
}

pub fn read_bytes(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = fs::File::open(path)
        .with_context(|| format!("{}: {}", tr("importer.read_failed"), path.display()))?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "{}",
        tr("importer.source_too_large")
    );
    Ok(bytes)
}

pub fn decode_text(bytes: &[u8]) -> Result<String> {
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        ensure!(
            bytes.len().is_multiple_of(2),
            "{}",
            tr("importer.invalid_source")
        );
        let little = bytes[0] == 0xff;
        let words: Vec<u16> = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| {
                if little {
                    u16::from_le_bytes([b[0], b[1]])
                } else {
                    u16::from_be_bytes([b[0], b[1]])
                }
            })
            .collect();
        return Ok(String::from_utf16(&words)?);
    }
    Ok(std::str::from_utf8(bytes)?
        .trim_start_matches('\u{feff}')
        .to_owned())
}

pub fn json(path: &Path) -> Result<Value> {
    serde_json::from_str(&decode_text(&read_bytes(path, 32 * 1024 * 1024)?)?)
        .with_context(|| format!("{}: {}", tr("importer.invalid_source"), path.display()))
}

/// Source databases are opened read-only; the Online Backup API includes committed WAL data.
pub fn snapshot(path: &Path, temporary: &Path, index: usize) -> Result<Connection> {
    let original = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("{}: {}", tr("importer.snapshot_failed"), path.display()))?;
    original.busy_timeout(Duration::from_secs(5))?;
    // Acquire the source read transaction with the bounded busy handler before the backup
    // loop; exclusive source locks then fail with actionable context rather than spin.
    original
        .execute_batch("BEGIN; SELECT count(*) FROM sqlite_schema;")
        .with_context(|| format!("{}: {}", tr("importer.snapshot_failed"), path.display()))?;
    let target = temporary.join(format!("snapshot-{index}.db"));
    let mut destination = Connection::open(&target)?;
    Backup::new(&original, &mut destination)?
        .run_to_completion(256, Duration::from_millis(20), None)
        .with_context(|| format!("{}: {}", tr("importer.snapshot_failed"), path.display()))?;
    drop(destination);
    Ok(Connection::open_with_flags(
        target,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

pub fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
        |r| r.get(0),
    )?)
}

pub fn visit_rows(
    conn: &Connection,
    table: &str,
    mut visit: impl FnMut(Value) -> Result<()>,
) -> Result<usize> {
    // Table selectors are internal constants, never supplied by a source row or CLI argument.
    ensure!(
        [
            "clipboard_items",
            "clipboard_groups",
            "clipboard_apps",
            "file_type_icons",
            "history"
        ]
        .contains(&table),
        "{}",
        tr("importer.invalid_source")
    );
    let mut statement = conn.prepare(&format!("SELECT * FROM {table}"))?;
    let names: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut rows = statement.query([])?;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        let mut record = Map::new();
        for (index, name) in names.iter().enumerate() {
            let value = match row.get_ref(index)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(v) => Value::from(v),
                ValueRef::Real(v) => Value::from(v),
                ValueRef::Text(v) => Value::from(std::str::from_utf8(v)?),
                ValueRef::Blob(v) => serde_json::to_value(v)?,
            };
            record.insert(name.clone(), value);
        }
        visit(Value::Object(record))?;
        count += 1;
    }
    Ok(count)
}

pub fn discover(selected: Option<Kind>, custom: &[Input]) -> Result<Vec<Input>> {
    let mut candidates = Vec::new();
    let mut add = |kind, path: PathBuf| {
        if selected.is_none_or(|s| s == kind)
            && !custom.iter().any(|input| input.kind == kind)
            && path.exists()
        {
            candidates.push(Input { kind, path });
        }
    };
    if let Some(appdata) = std::env::var_os("APPDATA").map(PathBuf::from) {
        add(Kind::Clipboard, appdata.join("clipboard"));
        add(Kind::Audio, appdata.join("audio"));
        add(Kind::Monitor, appdata.join("monitor"));
        add(Kind::Everything, appdata.join("Everything"));
        add(Kind::Vision, appdata.join("vision"));
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        add(Kind::Clipboard, local.join("clipboard/prod"));
        add(Kind::Vision, local.join("vision"));
        add(Kind::Vision, local.join("Programs/vision"));
    }
    let mut portable = vec![std::env::current_dir()?];
    if let Some(parent) = std::env::current_exe()?.parent() {
        portable.push(parent.to_owned());
    }
    if let Some(path) = std::env::var_os("PATH") {
        portable.extend(std::env::split_paths(&path));
    }
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(root) = std::env::var_os(variable).map(PathBuf::from) {
            for app in ["clipboard", "audio", "monitor", "vision", "Everything"] {
                portable.push(root.join(app));
            }
        }
    }
    for root in portable {
        for (kind, probes) in [
            (
                Kind::Clipboard,
                &[
                    "db/clipboard.db",
                    "legacy_clipboard.db",
                    "config/settings.json",
                    ".store.json",
                    "storage.json",
                ][..],
            ),
            (Kind::Audio, &["legacy_audio.json"][..]),
            (
                Kind::Monitor,
                &[
                    "monitor.exe",
                    "legacy_monitor.ini",
                    "history_traffic.dat",
                    "history_traffic.dat.bak",
                    "history_traffic.bak",
                ][..],
            ),
            (
                Kind::Vision,
                &["Config/legacy_vision.json", "legacy_vision.json"][..],
            ),
            (Kind::Everything, &["Run History.csv"][..]),
        ] {
            if probes.iter().any(|p| root.join(p).is_file()) {
                add(kind, root.clone());
            }
        }
    }
    for input in custom {
        ensure!(
            input.path.exists(),
            "{}: {}",
            tr("importer.source_missing"),
            input.path.display()
        );
        if selected.is_none_or(|s| s == input.kind) {
            candidates.push(input.clone());
        }
    }
    let mut seen = BTreeSet::new();
    candidates.retain(|input| {
        seen.insert((
            input.kind,
            fs::canonicalize(&input.path).unwrap_or_else(|_| input.path.clone()),
        ))
    });
    Ok(candidates)
}

pub fn files(input: &Input) -> Result<Vec<PathBuf>> {
    if input.path.is_file() {
        return Ok(vec![input.path.clone()]);
    }
    let root = &input.path;
    let relative: &[&str] = match input.kind {
        Kind::Clipboard => &[
            "db/clipboard.db",
            "legacy_clipboard.db",
            "config/settings.json",
            ".store.json",
        ],
        Kind::Audio => &["legacy_audio.json"],
        Kind::Monitor => &[
            "config.ini",
            "legacy_monitor.ini",
            "history_traffic.dat",
            "history_traffic.dat.bak",
            "history_traffic.bak",
        ],
        Kind::Vision => &[
            "legacy_vision.json",
            "Config/legacy_vision.json",
            "RightMenu/settings.json",
            "Config/RightMenu/settings.json",
        ],
        Kind::Everything => &["Run History.csv"],
    };
    let mut files: Vec<_> = relative
        .iter()
        .map(|p| root.join(p))
        .filter(|p| p.is_file())
        .collect();
    if input.kind == Kind::Clipboard {
        let manifest = root.join("storage.json");
        if manifest.is_file()
            && let Some(path) = json(&manifest)?
                .get("dataDir")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        {
            let path = PathBuf::from(path);
            let path = if path.is_absolute() {
                path
            } else {
                root.join(path)
            };
            for name in ["db/clipboard.db", "config/settings.json"] {
                let file = path.join(name);
                if file.is_file() {
                    files.push(file);
                }
            }
        }
        let store = root.join(".store.json");
        if store.is_file()
            && let Some(path) = json(&store)?
                .pointer("/globalStore/env/saveDataDir")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        {
            let path = PathBuf::from(path);
            let path = if path.is_absolute() {
                path
            } else {
                root.join(path)
            };
            let file = path.join("legacy_clipboard.db");
            if file.is_file() {
                files.push(file);
            }
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_parse_and_serialize_neutral_names() -> Result<()> {
        for (name, kind) in [
            ("clipboard", Kind::Clipboard),
            ("audio", Kind::Audio),
            ("monitor", Kind::Monitor),
            ("vision", Kind::Vision),
            ("everything", Kind::Everything),
        ] {
            assert_eq!(Kind::parse(name)?, kind);
            assert_eq!(serde_json::to_value(kind)?, Value::from(name));
        }
        for name in ["", "all", "Clipboard", "capture", "sound", "network"] {
            assert!(Kind::parse(name).is_err(), "{name}");
        }
        Ok(())
    }

    #[test]
    fn discovers_neutral_files_for_each_source_kind() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path();
        let fixtures = [
            (
                Kind::Clipboard,
                &[
                    "db/clipboard.db",
                    "legacy_clipboard.db",
                    "config/settings.json",
                    ".store.json",
                ][..],
            ),
            (Kind::Audio, &["legacy_audio.json"][..]),
            (
                Kind::Monitor,
                &[
                    "config.ini",
                    "legacy_monitor.ini",
                    "history_traffic.dat",
                    "history_traffic.dat.bak",
                    "history_traffic.bak",
                ][..],
            ),
            (
                Kind::Vision,
                &[
                    "legacy_vision.json",
                    "Config/legacy_vision.json",
                    "RightMenu/settings.json",
                    "Config/RightMenu/settings.json",
                ][..],
            ),
            (Kind::Everything, &["Run History.csv"][..]),
        ];
        for (_, names) in &fixtures {
            for name in *names {
                let path = root.join(name);
                fs::create_dir_all(path.parent().unwrap())?;
                fs::write(path, "{}")?;
            }
        }
        fs::write(root.join("unrelated.json"), "{}")?;
        for (kind, names) in fixtures {
            let input = Input {
                kind,
                path: root.to_owned(),
            };
            let sources = discover(Some(kind), &[input.clone(), input])?;
            assert_eq!(sources.len(), 1);
            assert_eq!(sources[0].kind, kind);
            assert_eq!(
                files(&sources[0])?,
                names.iter().map(|name| root.join(name)).collect::<Vec<_>>()
            );
        }
        Ok(())
    }

    #[test]
    fn discovers_relocated_clipboard_files_from_legacy_schema_fields() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let root = temporary.path();
        for name in [
            "modern/db/clipboard.db",
            "modern/config/settings.json",
            "legacy/legacy_clipboard.db",
        ] {
            let path = root.join(name);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, "{}")?;
        }
        fs::write(root.join("storage.json"), r#"{"dataDir":"modern"}"#)?;
        fs::write(
            root.join(".store.json"),
            r#"{"globalStore":{"env":{"saveDataDir":"legacy"}}}"#,
        )?;
        assert_eq!(
            files(&Input {
                kind: Kind::Clipboard,
                path: root.to_owned(),
            })?,
            [
                ".store.json",
                "modern/db/clipboard.db",
                "modern/config/settings.json",
                "legacy/legacy_clipboard.db",
            ]
            .map(|name| root.join(name))
            .to_vec()
        );
        Ok(())
    }
}
