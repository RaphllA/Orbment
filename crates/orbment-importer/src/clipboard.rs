use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Local, NaiveDateTime, SecondsFormat, TimeZone};
use orbment_core::{
    clipboard_content::{
        content_hash, html_plain, image_relative_path, is_sensitive, rtf_plain,
        truncate_search_text,
    },
    i18n::tr,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    io::Cursor,
    path::{Component, Path, PathBuf},
};

#[derive(Default, Serialize)]
pub struct ClipboardReport {
    pub source_items: usize,
    pub inserted_items: usize,
    pub duplicate_items: usize,
    pub skipped_items: usize,
    pub image_references: usize,
    pub missing_images: Vec<PathBuf>,
    pub invalid_images: Vec<PathBuf>,
    pub missing_icons: Vec<PathBuf>,
    pub unsupported_items: Vec<String>,
}

pub struct Media {
    staging: PathBuf,
    target: PathBuf,
    files: BTreeMap<String, PathBuf>,
    next_icon: usize,
}
impl Media {
    pub fn new(staging: PathBuf, target: PathBuf) -> Result<Self> {
        fs::create_dir_all(&staging)?;
        Ok(Self {
            staging,
            target,
            files: BTreeMap::new(),
            next_icon: 0,
        })
    }
    fn stage(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        if !self.files.contains_key(name) {
            let file = self.staging.join(format!("{}.media", self.files.len()));
            fs::write(&file, bytes)?;
            self.files.insert(name.to_owned(), file);
        }
        Ok(())
    }
    fn image(&mut self, source: &Path) -> Result<(String, i64, u32, u32)> {
        let original = crate::source::read_bytes(source, 100 * 1024 * 1024)?;
        let mut reader = image::ImageReader::new(Cursor::new(&original)).with_guessed_format()?;
        let is_png = reader.format() == Some(image::ImageFormat::Png);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(32768);
        limits.max_image_height = Some(32768);
        limits.max_alloc = Some(256 * 1024 * 1024);
        reader.limits(limits);
        let image = reader.decode()?;
        let bytes = if is_png {
            original
        } else {
            let mut encoded = Cursor::new(Vec::new());
            image.write_to(&mut encoded, image::ImageFormat::Png)?;
            encoded.into_inner()
        };
        let name = image_relative_path(&bytes);
        self.stage(&name, &bytes)?;
        Ok((
            name,
            i64::try_from(bytes.len())?,
            image.width(),
            image.height(),
        ))
    }
    fn icon(&mut self, source: &Path) -> Result<String> {
        let bytes = crate::source::read_bytes(source, 32 * 1024 * 1024)?;
        let extension = source.extension().and_then(|v| v.to_str()).unwrap_or("bin");
        ensure!(
            extension.chars().all(|c| c.is_ascii_alphanumeric()),
            "{}",
            tr("importer.invalid_source")
        );
        loop {
            self.next_icon += 1;
            let name = format!("media/app_icons/import-{}.{extension}", self.next_icon);
            let existing = self.target.join(&name);
            if existing.exists() {
                if crate::source::read_bytes(&existing, 32 * 1024 * 1024)? == bytes {
                    return Ok(name);
                }
                continue;
            }
            self.stage(&name, &bytes)?;
            return Ok(name);
        }
    }
    pub fn publish(&self) -> Result<()> {
        for (relative, staged) in &self.files {
            let path = self.target.join(relative);
            if path.exists() {
                ensure!(
                    fs::read(&path)? == fs::read(staged)?,
                    "{}: {}",
                    tr("importer.media_conflict"),
                    path.display()
                );
            } else {
                let parent = path.parent().context(tr("importer.invalid_source"))?;
                fs::create_dir_all(parent)?;
                let mut output = tempfile::NamedTempFile::new_in(parent)?;
                std::io::copy(&mut fs::File::open(staged)?, output.as_file_mut())?;
                output.as_file().sync_all()?;
                output.persist_noclobber(&path)?;
            }
        }
        Ok(())
    }
}

fn string<'a>(row: &'a Value, field: &str) -> Result<&'a str> {
    row[field]
        .as_str()
        .with_context(|| format!("{}: {field}", tr("importer.invalid_source")))
}
fn number(row: &Value, field: &str, default: i64) -> i64 {
    row[field]
        .as_i64()
        .or_else(|| row[field].as_bool().map(i64::from))
        .unwrap_or(default)
}
fn timestamp(value: &str) -> Result<String> {
    let utc = if let Ok(time) = DateTime::parse_from_rfc3339(value) {
        time.to_utc()
    } else {
        let naive = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
            .or_else(|_| NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f"))?;
        // Legacy timestamps are local wall time; ambiguous DST instants must not be invented.
        Local
            .from_local_datetime(&naive)
            .single()
            .context(tr("importer.ambiguous_timestamp"))?
            .to_utc()
    };
    Ok(utc.to_rfc3339_opts(SecondsFormat::Millis, true))
}
fn image_path(root: &Path, modern: bool, content: &str) -> Result<PathBuf> {
    ensure!(
        !content.is_empty()
            && content != "."
            && content != ".."
            && !content.contains(['/', '\\', ':', '\0']),
        "{}: image",
        tr("importer.invalid_source")
    );
    if modern {
        let shard = content.get(..2).context(tr("importer.invalid_source"))?;
        ensure!(
            shard.bytes().all(|v| v.is_ascii_hexdigit()),
            "{}: image",
            tr("importer.invalid_source")
        );
        Ok(root
            .join("resources/clipboard-images/origin")
            .join(shard)
            .join(content))
    } else {
        Ok(root.join("images").join(content))
    }
}
fn icon_path(root: &Path, app: bool, content: &str) -> Result<PathBuf> {
    let normalized = content.replace('\\', "/");
    let path = Path::new(&normalized);
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    ensure!(
        !normalized.contains([':', '\0'])
            && path.components().all(|c| matches!(c, Component::Normal(_))),
        "{}: icon",
        tr("importer.invalid_source")
    );
    let directory = if app { "app-icons" } else { "file-icons" };
    Ok(if path.starts_with(format!("resources/{directory}")) {
        root.join(path)
    } else if path.starts_with(directory) {
        root.join("resources").join(path)
    } else {
        root.join("resources").join(directory).join(path)
    })
}

fn html_clipboard_payload(html: &str) -> Vec<u8> {
    if html.starts_with("Version:") {
        return html.as_bytes().to_vec();
    }
    // The source stores an HTML document, whereas Windows HTML Format requires byte offsets.
    let header = |start: usize, end: usize, fragment_start: usize, fragment_end: usize| {
        format!(
            "Version:1.0\r\nStartHTML:{start:010}\r\nEndHTML:{end:010}\r\nStartFragment:{fragment_start:010}\r\nEndFragment:{fragment_end:010}\r\n"
        )
    };
    let start = header(0, 0, 0, 0).len();
    let lower = html.to_ascii_lowercase();
    let fragment_start = lower
        .find("<!--startfragment-->")
        .map_or(0, |offset| offset + "<!--startfragment-->".len());
    let fragment_end = lower.find("<!--endfragment-->").unwrap_or(html.len());
    let mut payload = header(
        start,
        start + html.len(),
        start + fragment_start,
        start + fragment_end,
    )
    .into_bytes();
    payload.extend_from_slice(html.as_bytes());
    payload.push(0);
    payload
}

pub fn import(
    conn: &Connection,
    source: &Connection,
    database: &Path,
    modern: bool,
    media: &mut Media,
    report: &mut ClipboardReport,
) -> Result<()> {
    let parent = database.parent().context(tr("importer.invalid_source"))?;
    let root = if modern && parent.file_name().is_some_and(|v| v == "db") {
        parent.parent().context(tr("importer.invalid_source"))?
    } else {
        parent
    };
    let mut apps = BTreeMap::new();
    if modern {
        if crate::source::table_exists(source, "clipboard_groups")? {
            crate::source::visit_rows(source, "clipboard_groups", |row| {
                conn.execute("INSERT INTO clipboard_groups(id,name,icon,is_hidden,sort_order,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(id) DO NOTHING",
                    params![string(&row,"id")?, string(&row,"name")?, string(&row,"icon")?, number(&row,"is_hidden",0), number(&row,"sort_order",0), timestamp(string(&row,"created_at")?)?, timestamp(string(&row,"updated_at")?)?])?;
                Ok(())
            })?;
        }
        for table in ["clipboard_apps", "file_type_icons"] {
            if !crate::source::table_exists(source, table)? {
                continue;
            }
            crate::source::visit_rows(source, table, |row| {
                let platform = row["platform"].as_str().unwrap_or("windows");
                if table == "clipboard_apps" {
                    let id = string(&row, "id")?;
                    let executable = row["bundle_or_exe"].as_str();
                    let existing: Option<String> = conn.query_row(
                        "SELECT id FROM clipboard_apps WHERE id=?1 OR (?2 IS NOT NULL AND bundle_or_exe=?2) ORDER BY id=?1 DESC LIMIT 1",
                        params![id, executable], |r| r.get(0)).optional()?;
                    if let Some(existing) = existing {
                        apps.insert(id.to_owned(), existing);
                        return Ok(());
                    }
                } else if conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM file_type_icons WHERE cache_key=?1 AND platform=?2)",
                    params![string(&row, "cache_key")?, platform],
                    |r| r.get::<_, bool>(0),
                )? {
                    return Ok(());
                }
                let icon = if let Some(value) = row["icon_file"].as_str().filter(|s| !s.is_empty())
                {
                    let path = icon_path(root, table == "clipboard_apps", value)?;
                    if path.is_file() {
                        Some(media.icon(&path)?)
                    } else {
                        report.missing_icons.push(path);
                        None
                    }
                } else {
                    None
                };
                let created = timestamp(string(&row, "created_at")?)?;
                let updated = timestamp(string(&row, "updated_at")?)?;
                if table == "clipboard_apps" {
                    let id = string(&row, "id")?;
                    let executable = row["bundle_or_exe"].as_str();
                    conn.execute("INSERT INTO clipboard_apps(id,name,bundle_or_exe,icon_file,platform,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                        params![id,string(&row,"name")?,executable,icon,platform,created,updated])?;
                    apps.insert(id.to_owned(), id.to_owned());
                } else if let Some(icon) = icon {
                    conn.execute("INSERT INTO file_type_icons(cache_key,platform,icon_file,created_at,updated_at) VALUES(?1,?2,?3,?4,?5)",
                        params![string(&row,"cache_key")?,platform,icon,created,updated])?;
                }
                Ok(())
            })?;
        }
    }
    crate::source::visit_rows(
        source,
        if modern { "clipboard_items" } else { "history" },
        |row| {
            report.source_items += 1;
            let id = string(&row, "id")?;
            let source_kind = string(&row, if modern { "kind" } else { "type" })?;
            let (kind, sub_kind) = match source_kind {
                "html" | "rtf" => ("text", Some(source_kind)),
                "text" | "image" | "files" => (
                    source_kind,
                    row[if modern { "sub_kind" } else { "subtype" }].as_str(),
                ),
                _ => {
                    report
                        .unsupported_items
                        .push(format!("{}:{id}:{source_kind}", database.display()));
                    return Ok(());
                }
            };
            let source_content = string(&row, if modern { "content" } else { "value" })?;
            let mut content = source_content.to_owned();
            let mut size = row["size"].as_i64();
            let mut width = row["width"].as_i64();
            let mut height = row["height"].as_i64();
            let mut raw: Option<Vec<u8>> = match &row["raw_payload"] {
                Value::Array(_) => Some(serde_json::from_value(row["raw_payload"].clone())?),
                Value::String(v) => Some(v.as_bytes().to_vec()),
                _ => None,
            };
            if matches!(sub_kind, Some("html" | "rtf")) && raw.is_none() {
                let payload = if sub_kind == Some("html") {
                    html_clipboard_payload(source_content)
                } else {
                    source_content.as_bytes().to_vec()
                };
                content = if sub_kind == Some("html") {
                    html_plain(&payload)?
                } else {
                    rtf_plain(&payload)?
                };
                raw = Some(payload);
            }
            if kind == "files" {
                // Modern clipboard storage uses LF-separated paths; v0.5 history.value is JSON.
                let files: Vec<String> = if modern {
                    source_content.lines().map(str::to_owned).collect()
                } else {
                    serde_json::from_str(source_content)?
                };
                ensure!(
                    !files.is_empty() && files.iter().all(|s| !s.is_empty() && !s.contains('\0')),
                    "{}: {id}",
                    tr("importer.invalid_source")
                );
                content = serde_json::to_string(&files)?;
            }
            if kind == "image" {
                report.image_references += 1;
                let path = image_path(root, modern, source_content)?;
                if !path.is_file() {
                    report.missing_images.push(path);
                    return Ok(());
                }
                match media.image(&path) {
                    Ok((relative, bytes, image_width, image_height)) => {
                        content = relative;
                        size = Some(bytes);
                        width = Some(i64::from(image_width));
                        height = Some(i64::from(image_height));
                    }
                    Err(error) => {
                        if error.is::<image::ImageError>() {
                            report.invalid_images.push(path);
                            return Ok(());
                        }
                        return Err(error);
                    }
                }
            }
            let hash = content_hash(kind, &content);
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM clipboard_items WHERE id=?1 OR content_hash=?2)",
                params![id, hash],
                |r| r.get(0),
            )?;
            if exists {
                report.duplicate_items += 1;
                return Ok(());
            }
            let group = row["group_id"].as_str();
            let group = if let Some(group) = group {
                conn.query_row(
                    "SELECT id FROM clipboard_groups WHERE id=?1",
                    [group],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
            } else {
                None
            };
            let app = row["source_app_id"].as_str().and_then(|id| apps.get(id));
            let search = row[if modern { "search_text" } else { "search" }]
                .as_str()
                .filter(|v| !v.is_empty())
                .unwrap_or(if kind == "image" { "" } else { &content });
            let created = timestamp(string(
                &row,
                if modern { "created_at" } else { "createTime" },
            )?)?;
            let updated = if modern {
                timestamp(string(&row, "updated_at")?)?
            } else {
                created.clone()
            };
            let sensitive = i64::from(
                number(&row, "is_sensitive", 0) != 0
                    || [
                        &content,
                        search,
                        row["summary"].as_str().unwrap_or(""),
                        row["note"].as_str().unwrap_or(""),
                    ]
                    .into_iter()
                    .any(is_sensitive),
            );
            if kind == "text" {
                size = Some(i64::try_from(content.encode_utf16().count())?);
            }
            conn.execute("INSERT INTO clipboard_items(id,kind,sub_kind,group_id,source_app_id,content,raw_payload,content_hash,search_text,summary,file_types,size,width,height,use_count,is_favorite,is_pinned,is_sensitive,platform,note,created_at,updated_at)
            VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)",
            params![id,kind,sub_kind,group,app,content,raw,hash,truncate_search_text(search),row["summary"].as_str(),row["file_types"].as_str(),size,width,height,number(&row,"use_count",1),number(&row,if modern {"is_favorite"} else {"favorite"},0),number(&row,"is_pinned",0),sensitive,row["platform"].as_str().unwrap_or("windows"),row["note"].as_str(),created,updated])?;
            report.inserted_items += 1;
            Ok(())
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_app_and_file_type_icons_are_not_staged_or_published() -> Result<()> {
        let root = tempfile::tempdir()?;
        let source_root = root.path().join("source");
        let target = root.path().join("target");
        let mut source = Connection::open_in_memory()?;
        let mut conn = Connection::open_in_memory()?;
        orbment_core::db::initialize(&mut source)?;
        orbment_core::db::initialize(&mut conn)?;
        let timestamp = "2026-10-24T00:00:00Z";
        for (id, executable) in [("same-id", "preserved.exe"), ("canonical", "existing.exe")] {
            conn.execute(
                "INSERT INTO clipboard_apps(id,name,bundle_or_exe,icon_file,created_at,updated_at)
                 VALUES(?1,?1,?2,'preserved.ico',?3,?3)",
                params![id, executable, timestamp],
            )?;
        }
        conn.execute(
            "INSERT INTO file_type_icons(cache_key,icon_file,created_at,updated_at)
             VALUES('txt','preserved.ico',?1,?1)",
            [timestamp],
        )?;
        for (id, executable, icon) in [
            ("same-id", "incoming.exe", "unused.ico"),
            ("alias", "EXISTING.exe", "unused.ico"),
            ("new-app", "new.exe", "new.ico"),
        ] {
            source.execute(
                "INSERT INTO clipboard_apps(id,name,bundle_or_exe,icon_file,created_at,updated_at)
                 VALUES(?1,?1,?2,?3,?4,?4)",
                params![id, executable, icon, timestamp],
            )?;
            source.execute(
                "INSERT INTO clipboard_items(id,kind,source_app_id,content,content_hash,created_at,updated_at)
                 VALUES(?1,'text',?1,?1,?1,?2,?2)",
                params![id, timestamp],
            )?;
        }
        for (key, icon) in [("txt", "unused.ico"), ("rs", "new.ico")] {
            source.execute(
                "INSERT INTO file_type_icons(cache_key,icon_file,created_at,updated_at)
                 VALUES(?1,?2,?3,?3)",
                params![key, icon, timestamp],
            )?;
        }
        for directory in ["app-icons", "file-icons"] {
            let path = source_root.join("resources").join(directory);
            fs::create_dir_all(&path)?;
            fs::write(path.join("unused.ico"), b"unreferenced icon")?;
            fs::write(path.join("new.ico"), directory.as_bytes())?;
        }
        let mut media = Media::new(root.path().join("staging"), target.clone())?;
        let mut report = ClipboardReport::default();
        import(
            &conn,
            &source,
            &source_root.join("clipboard.db"),
            true,
            &mut media,
            &mut report,
        )?;
        assert_eq!(report.inserted_items, 3);
        assert!(report.missing_icons.is_empty());
        let mapped: String = conn.query_row(
            "SELECT source_app_id FROM clipboard_items WHERE id='alias'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(mapped, "canonical");
        let preserved: i64 = conn.query_row(
            "SELECT COUNT(*) FROM clipboard_apps WHERE icon_file='preserved.ico'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(preserved, 2);
        assert_eq!(media.files.len(), 2);
        media.publish()?;
        let mut statement = conn.prepare(
            "SELECT icon_file FROM clipboard_apps WHERE id='new-app'
             UNION ALL SELECT icon_file FROM file_type_icons WHERE cache_key='rs'",
        )?;
        for relative in statement.query_map([], |row| row.get::<_, String>(0))? {
            assert!(target.join(relative?).is_file());
        }
        assert_eq!(fs::read_dir(target.join("media/app_icons"))?.count(), 2);
        let preserved: String = conn.query_row(
            "SELECT icon_file FROM file_type_icons WHERE cache_key='txt'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(preserved, "preserved.ico");

        fs::remove_dir_all(source_root.join("resources"))?;
        let mut media = Media::new(root.path().join("repeat-staging"), target)?;
        let mut report = ClipboardReport::default();
        import(
            &conn,
            &source,
            &source_root.join("clipboard.db"),
            true,
            &mut media,
            &mut report,
        )?;
        assert_eq!(report.duplicate_items, 3);
        assert!(report.missing_icons.is_empty());
        assert!(media.files.is_empty());
        Ok(())
    }
}
