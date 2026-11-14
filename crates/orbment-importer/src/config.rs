use anyhow::{Context, Result};
use orbment_core::{config::ConfigStore, i18n::tr};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};
use toml_edit::DocumentMut;

mod audio;
mod monitor;
mod vision;

#[derive(Default, Serialize)]
pub struct ConfigReport {
    pub mapped: Vec<String>,
    pub conflicts: Vec<String>,
    pub skipped_existing: Vec<String>,
    pub unmapped: Vec<String>,
}
pub struct ConfigPlan {
    pub store: ConfigStore,
    original: DocumentMut,
    pub report: ConfigReport,
    only_unset: bool,
    pub target: PathBuf,
}
impl ConfigPlan {
    pub fn new(target: &Path, scratch: &Path, only_unset: bool) -> Result<Self> {
        let path = target.join("config.toml");
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        fs::create_dir_all(scratch)?;
        fs::write(scratch.join("config.toml"), &text)?;
        Ok(Self {
            store: ConfigStore::load(scratch)?,
            original: text.parse()?,
            report: ConfigReport::default(),
            only_unset,
            target: path,
        })
    }
    pub fn import(&mut self, path: &Path, kind: crate::source::Kind) -> Result<()> {
        let value = if kind == crate::source::Kind::Monitor {
            monitor::parse_ini(&monitor::decode_ini(&crate::source::read_bytes(
                path,
                32 * 1024 * 1024,
            )?)?)?
        } else {
            crate::source::json(path)?
        };
        let mut mapper = Mapper {
            source: &value,
            changes: Vec::new(),
            consumed: BTreeSet::new(),
        };
        match kind {
            crate::source::Kind::Clipboard => {
                if path.file_name().is_some_and(|p| p == ".store.json") {
                    legacy(&mut mapper);
                } else {
                    modern(&mut mapper, &self.store.get("clipboard")?);
                }
            }
            crate::source::Kind::Audio => audio::map(&mut mapper),
            crate::source::Kind::Monitor => {
                monitor::map(&mut mapper);
                taskbar_options(&mut mapper);
            }
            crate::source::Kind::Vision => vision::map(&mut mapper, path),
            crate::source::Kind::Everything => return Ok(()),
        }
        let label = path.display().to_string();
        let mapped_before = self.report.mapped.len();
        for (source, key, value) in mapper.changes {
            let mut item = Some(self.original.as_item());
            for part in key.split('.') {
                item = item.and_then(|i| i.get(part));
            }
            let explicit = item.is_some();
            let current = self.store.get(&key);
            if explicit && current.as_ref().is_ok_and(|v| *v != value) {
                self.report
                    .conflicts
                    .push(format!("{label}:{source} -> {key}"));
            }
            if self.only_unset && explicit {
                self.report.skipped_existing.push(key);
                continue;
            }
            match self.store.apply_json(&key, value) {
                Ok(()) => self
                    .report
                    .mapped
                    .push(format!("{label}:{source} -> {key}")),
                Err(_) => self.report.unmapped.push(format!(
                    "{label}:{source} -> {key}: {}",
                    tr("importer.incompatible_setting")
                )),
            }
        }
        if self.report.mapped.len() > mapped_before {
            self.store.save().with_context(|| {
                format!(
                    "{}: {}",
                    tr("config.write_failed"),
                    self.store.path().display()
                )
            })?;
        }
        let mut leaves = Vec::new();
        leaf_paths(&value, "", &mut leaves);
        for leaf in leaves {
            if !mapper
                .consumed
                .iter()
                .any(|p| leaf == *p || leaf.starts_with(&format!("{p}/")))
            {
                self.report.unmapped.push(format!("{label}:{leaf}"));
            }
        }
        Ok(())
    }
    pub fn publish(&self) -> Result<()> {
        if !self.report.mapped.is_empty() {
            orbment_core::config::atomic_write(&self.target, &fs::read(self.store.path())?)?;
        }
        Ok(())
    }
}

struct Mapper<'a> {
    source: &'a Value,
    changes: Vec<(String, String, Value)>,
    consumed: BTreeSet<String>,
}
impl Mapper<'_> {
    fn get(&self, source: &str) -> Option<&Value> {
        self.source.pointer(source)
    }
    fn put(&mut self, source: &str, target: &str, value: Value) {
        self.changes
            .push((source.to_owned(), target.to_owned(), value));
        self.consumed.insert(source.to_owned());
    }
    fn copy(&mut self, source: &str, target: &str) {
        if let Some(value) = self.get(source).cloned().filter(|v| !v.is_null()) {
            self.put(source, target, value);
        }
    }
    fn pairs(&mut self, pairs: &[(&str, &str)]) {
        for (source, target) in pairs {
            self.copy(source, target);
        }
    }
    fn choice(&mut self, source: &str, target: &str, choices: &[(&str, &str)]) {
        if let Some(value) = self.get(source) {
            let text = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            if let Some((_, mapped)) = choices.iter().find(|(key, _)| *key == text) {
                self.put(source, target, json!(mapped));
            }
        }
    }
}

fn taskbar_options(map: &mut Mapper<'_>) {
    for field in [
        "transparent_background",
        "auto_set_background_color",
        "auto_adapt_light_theme",
        "horizontal_arrange",
        "value_right_align",
        "speed_short_mode",
        "separate_value_unit_with_space",
        "show_tool_tip",
        "unit_byte",
        "hide_unit",
        "hide_percent",
        "show_status_bar",
        "show_netspeed_figure",
        "cm_graph_type",
        "show_graph_dashed_box",
        "graph_color_following_system",
    ] {
        let source = if field == "transparent_background" {
            "/other/taksbar_transparent_color_enable".to_owned()
        } else {
            format!("/task_bar/{field}")
        };
        let value = match map.get(&source).and_then(Value::as_str) {
            Some("1" | "true") => Some(true),
            Some("0" | "false") => Some(false),
            _ => None,
        };
        if let Some(value) = value {
            map.put(&source, &format!("monitor.taskbar.{field}"), json!(value));
        }
    }
    for field in [
        "digits_number",
        "memory_display_mode",
        "item_space",
        "vertical_margin",
        "speed_unit",
        "double_click_action",
        "netspeed_figure_max_value",
        "netspeed_figure_max_value_unit",
        "taskbar_left_space_win11",
        "taskbar_right_space_win11",
    ] {
        let source = format!("/task_bar/{field}");
        if let Some(value) = map
            .get(&source)
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<i64>().ok())
        {
            map.put(&source, &format!("monitor.taskbar.{field}"), json!(value));
        }
    }
    map.copy(
        "/task_bar/double_click_exe",
        "monitor.taskbar.double_click_exe",
    );
    let source = "/task_bar/item_order";
    if let Some(order) = map.get(source).and_then(Value::as_str).and_then(|value| {
        value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::parse::<u8>)
            .collect::<Result<Vec<_>, _>>()
            .ok()
    }) {
        map.put(source, "monitor.taskbar.item_order", json!(order));
    }
}
fn leaf_paths(value: &Value, prefix: &str, output: &mut Vec<String>) {
    if let Some(object) = value.as_object().filter(|o| !o.is_empty()) {
        for (key, value) in object {
            leaf_paths(
                value,
                &format!("{prefix}/{}", key.replace('~', "~0").replace('/', "~1")),
                output,
            );
        }
    } else {
        output.push(prefix.to_owned());
    }
}
fn camel(value: &str) -> String {
    let mut uppercase = false;
    let mut result = String::new();
    for c in value.chars() {
        if c == '_' {
            uppercase = true;
        } else if uppercase {
            result.extend(c.to_uppercase());
            uppercase = false;
        } else {
            result.push(c);
        }
    }
    result
}
fn modern(map: &mut Mapper<'_>, defaults: &Value) {
    let mut fields = Vec::new();
    leaf_paths(defaults, "", &mut fields);
    for field in fields {
        let key = format!("clipboard{}", field.replace('/', "."));
        if matches!(
            field.as_str(),
            "/enabled" | "/open_clipboard_hotkey" | "/open_preference_hotkey" | "/replace_win_v"
        ) || field.starts_with("/quick_paste/")
        {
            continue;
        }
        let source = format!(
            "/clipboard{}",
            field.split('/').map(camel).collect::<Vec<_>>().join("/")
        );
        if field == "/preview/hover_delay_ms" {
            if let Some(value) = map.get(&source) {
                let delay = value
                    .as_u64()
                    .or_else(|| value.as_str()?.strip_prefix("ms")?.parse().ok());
                if let Some(delay) = delay {
                    map.put(&source, &key, json!(delay));
                }
            }
        } else {
            map.copy(&source, &key);
        }
    }
    map.pairs(&[
        (
            "/shortcuts/openClipboard",
            "clipboard.open_clipboard_hotkey",
        ),
        (
            "/shortcuts/openPreference",
            "clipboard.open_preference_hotkey",
        ),
        ("/shortcuts/winV", "clipboard.replace_win_v"),
        (
            "/clipboard/history/retention/value",
            "clipboard.history.retention_value",
        ),
        (
            "/clipboard/history/retention/unit",
            "clipboard.history.unit",
        ),
    ]);
}
fn legacy(map: &mut Mapper<'_>) {
    map.pairs(&[
        (
            "/globalStore/shortcut/clipboard",
            "clipboard.open_clipboard_hotkey",
        ),
        (
            "/globalStore/shortcut/quickPaste/enable",
            "clipboard.quick_paste.enabled",
        ),
        (
            "/clipboardStore/window/backTop",
            "clipboard.window.scroll_to_top_on_open",
        ),
        (
            "/clipboardStore/audio/copy",
            "clipboard.feedback.copy_sound",
        ),
        (
            "/clipboardStore/search/defaultFocus",
            "clipboard.search.default_focus",
        ),
        (
            "/clipboardStore/search/autoClear",
            "clipboard.search.clear_on_hide",
        ),
        ("/clipboardStore/content/ocr", "clipboard.capture.image_ocr"),
        (
            "/clipboardStore/content/copyPlain",
            "clipboard.content.copy_plain",
        ),
        (
            "/clipboardStore/content/pastePlain",
            "clipboard.content.paste_plain",
        ),
        (
            "/clipboardStore/content/autoFavorite",
            "clipboard.content.auto_favorite",
        ),
        (
            "/clipboardStore/content/deleteConfirm",
            "clipboard.content.delete_confirm",
        ),
        (
            "/clipboardStore/content/autoSort",
            "clipboard.content.update_on_reuse",
        ),
        (
            "/clipboardStore/history/maxCount",
            "clipboard.history.max_count",
        ),
    ]);
    map.choice(
        "/clipboardStore/window/position",
        "clipboard.window.position",
        &[
            ("follow", "followCursor"),
            ("remember", "remember"),
            ("center", "center"),
        ],
    );
    map.choice(
        "/clipboardStore/content/autoPaste",
        "clipboard.content.auto_paste",
        &[
            ("single", "singleClickPaste"),
            ("double", "doubleClickPaste"),
        ],
    );
    if let Some(value) = map
        .get("/globalStore/shortcut/quickPaste/value")
        .and_then(Value::as_str)
    {
        map.put(
            "/globalStore/shortcut/quickPaste/value",
            "clipboard.quick_paste.prefix",
            json!(
                value
                    .replace("CommandOrControl", "Ctrl")
                    .replace("Command", "Win")
                    .replace("Control", "Ctrl")
            ),
        );
    }
    if let Some(duration) = map
        .get("/clipboardStore/history/duration")
        .and_then(Value::as_u64)
        && (duration == 0
            || map
                .get("/clipboardStore/history/unit")
                .and_then(Value::as_u64)
                == Some(1))
    {
        map.put(
            "/clipboardStore/history/duration",
            "clipboard.history.retention_value",
            json!(duration),
        );
        map.put(
            "/clipboardStore/history/unit",
            "clipboard.history.unit",
            json!(if duration == 0 { "forever" } else { "days" }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_skips_incompatible_values_and_persists_valid_changes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let target = root.path().join("target");
        let scratch = root.path().join("scratch");
        fs::create_dir_all(&target)?;
        let original = "# retained comment\n[clipboard.display]\ntext_max_lines = 5\n";
        fs::write(target.join("config.toml"), original)?;
        let source = root.path().join("clipboard.json");
        fs::write(
            &source,
            json!({
                "clipboard": {
                    "display": {
                        "textMaxLines": 0,
                        "fileMaxCount": "invalid",
                        "imageMaxHeight": 96
                    },
                    "preview": {"hoverEnabled": true}
                }
            })
            .to_string(),
        )?;
        let mut plan = ConfigPlan::new(&target, &scratch, false)?;

        plan.import(&source, crate::source::Kind::Clipboard)?;

        assert_eq!(plan.report.mapped.len(), 2);
        assert_eq!(plan.report.unmapped.len(), 2);
        for key in [
            "clipboard.display.text_max_lines",
            "clipboard.display.file_max_count",
        ] {
            assert!(plan.report.unmapped.iter().any(|entry| {
                entry.contains(key) && entry.contains(&tr("importer.incompatible_setting"))
            }));
        }
        let staged = ConfigStore::load(&scratch)?;
        assert_eq!(staged.config.clipboard.display.text_max_lines, 5);
        assert_eq!(staged.config.clipboard.display.file_max_count, 3);
        assert_eq!(staged.config.clipboard.display.image_max_height, 96);
        assert!(staged.config.clipboard.preview.hover_enabled);
        assert_eq!(fs::read_to_string(&plan.target)?, original);

        plan.publish()?;
        assert!(fs::read_to_string(&plan.target)?.contains("# retained comment"));
        let published = ConfigStore::load(&target)?;
        assert_eq!(published.get("clipboard")?, staged.get("clipboard")?);
        Ok(())
    }

    #[test]
    fn import_propagates_atomic_write_creation_and_replacement_failures() -> Result<()> {
        for block_replacement in [false, true] {
            let root = tempfile::tempdir()?;
            let target = root.path().join("target");
            let scratch = root.path().join("scratch");
            fs::create_dir_all(&target)?;
            let original = "# target stays unchanged\n";
            fs::write(target.join("config.toml"), original)?;
            let source = root.path().join("clipboard.json");
            fs::write(
                &source,
                json!({"clipboard": {
                    "display": {"imageMaxHeight": 96},
                    "preview": {"hoverEnabled": true}
                }})
                .to_string(),
            )?;
            let mut plan = ConfigPlan::new(&target, &scratch, false)?;
            if block_replacement {
                fs::remove_file(plan.store.path())?;
                fs::create_dir(plan.store.path())?;
            } else {
                fs::create_dir(plan.store.path().with_extension("tmp"))?;
            }

            let error = plan
                .import(&source, crate::source::Kind::Clipboard)
                .unwrap_err();

            assert!(error.to_string().contains(&tr("config.write_failed")));
            assert!(plan.report.unmapped.is_empty());
            assert_eq!(plan.report.mapped.len(), 2);
            assert_eq!(fs::read_to_string(&plan.target)?, original);
        }
        Ok(())
    }

    #[test]
    fn imports_taskbar_layout_and_native_ini_types() -> Result<()> {
        let source = monitor::parse_ini(
            "[other]\ntaksbar_transparent_color_enable=1\n[task_bar]\nhorizontal_arrange=0\nitem_order=4,0,1\nitem_space=7\nvertical_margin=-2\ndigits_number=5\ndouble_click_exe=C:\\Tools\\monitor.exe\n",
        )?;
        let mut mapper = Mapper {
            source: &source,
            changes: Vec::new(),
            consumed: BTreeSet::new(),
        };
        taskbar_options(&mut mapper);
        for (field, expected) in [
            ("transparent_background", json!(true)),
            ("horizontal_arrange", json!(false)),
            ("item_order", json!([4, 0, 1])),
            ("item_space", json!(7)),
            ("vertical_margin", json!(-2)),
            ("digits_number", json!(5)),
            ("double_click_exe", json!("C:\\Tools\\monitor.exe")),
        ] {
            let key = format!("monitor.taskbar.{field}");
            assert!(
                mapper
                    .changes
                    .iter()
                    .any(|(_, target, value)| { target == &key && value == &expected }),
                "{key}"
            );
        }
        Ok(())
    }
}
