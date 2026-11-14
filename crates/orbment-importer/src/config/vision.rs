use super::Mapper;
use serde_json::{Value, json};
use std::path::Path;

pub(super) fn map(m: &mut Mapper<'_>, path: &Path) {
    if path.file_name().is_some_and(|v| v == "settings.json") {
        // Legacy shell-menu flags and executable/icon paths are intentionally not imported.
        return;
    }
    for (source, target) in [
        ("System.DesktopToolBar", "desktop_toolbar_mode"),
        ("System.AllowSimpleHotkey", "allow_simple_hotkey"),
        ("Shortcut.IgnoreProcessList", "ignore_process_list"),
        (
            "Screenshot.ConfirmOnCloseByKey",
            "screenshot.confirm_on_close_by_key",
        ),
        ("Screenshot.KeepMode", "screenshot.keep_mode"),
        ("Screenshot.enableRoundRect", "screenshot.enable_round_rect"),
        ("Screenshot.roundRectRatio", "screenshot.ratio"),
        ("MarkBar.Common.Color", "mark.common_color_argb"),
        ("MarkBar.Geometry.Filling", "mark.geometry.filling"),
        ("MarkBar.Geometry.PathMode", "mark.geometry.path_mode"),
        ("MarkBar.Text.Size", "mark.text.size"),
        ("MarkBar.Mosaic.MosaicStrength", "mark.mosaic.strength"),
        (
            "MarkBar.AutoMosaic.MosaicStrength",
            "mark.auto_mosaic.strength",
        ),
        ("Pin.ConfirmationFlags", "pin.confirmation_flags"),
        ("Pin.Image.ShowTextPopupMenu", "pin.show_text_popup_menu"),
        (
            "Pin.showTaskbarIconWhenUnpinned",
            "pin.show_taskbar_icon_when_unpinned",
        ),
        ("Ocr.RemoveSpaces", "ocr.remove_spaces"),
        ("Ocr.VerticalReadingOrder", "ocr.vertical_reading"),
        (
            "System.Text Recognition.CopyTextConfirmation",
            "ocr.copy_text_confirmation",
        ),
        (
            "Screenshot.Long Screenshot.AutoCrop",
            "long_shot.auto_crop_on_scroll_back",
        ),
        ("video.record.fps", "movie.fps"),
        ("video.playback.encodeFormat", "movie.encode_format"),
        (
            "video.record.recordToolbarDisplayType",
            "movie.toolbar_display_type",
        ),
        (
            "video.encode.exitRecordAfterEncode",
            "movie.exit_record_after_encode",
        ),
        ("Save.ManualSave", "save.manual_save_pattern"),
        ("Save.AutoSave.saveTypes", "save.auto_save_types_mask"),
        ("Save.SaveQuality", "save.save_quality"),
    ] {
        m.copy(&format!("/{source}/v"), &format!("vision.{target}"));
    }
    if let Some(groups) = m
        .get("/Mark.EditItemOrder/v/Vaild")
        .and_then(Value::as_array)
    {
        let order = groups
            .iter()
            .map(|group| {
                group["list"]
                    .as_array()?
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            })
            .collect::<Option<Vec<_>>>();
        if let Some(order) = order {
            m.put(
                "/Mark.EditItemOrder/v/Vaild",
                "vision.mark.edit_item_order",
                json!(order),
            );
        }
    }
    // The `t` metadata is not a setting. Deleted entries (`d`) and undocumented action
    // shortcut payloads remain unmapped, rather than guessing a private binding format.
    if let Some(object) = m.source.as_object() {
        for (key, value) in object {
            if value.get("t").is_some() {
                m.consumed
                    .insert(format!("/{}/t", key.replace('~', "~0").replace('/', "~1")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn ignores_legacy_shell_menu_settings() {
        for enabled in ["0", "1"] {
            let source = json!({ "Enabled": enabled });
            let mut mapper = Mapper {
                source: &source,
                changes: Vec::new(),
                consumed: BTreeSet::new(),
            };
            map(&mut mapper, Path::new("RightMenu/settings.json"));
            assert!(mapper.changes.is_empty());
            assert!(!mapper.consumed.contains("/Enabled"));
        }
    }

    #[test]
    fn maps_neutral_video_keys_and_legacy_value_wrappers() {
        let source = json!({
            "video.record.fps": { "v": 30, "t": "int" },
            "video.playback.encodeFormat": { "v": 1, "t": "int" },
            "video.record.recordToolbarDisplayType": { "v": 2, "t": "int" },
            "video.encode.exitRecordAfterEncode": { "v": true, "t": "bool" }
        });
        let mut mapper = Mapper {
            source: &source,
            changes: Vec::new(),
            consumed: BTreeSet::new(),
        };
        map(&mut mapper, Path::new("legacy_vision.json"));
        let changes: Vec<_> = mapper
            .changes
            .iter()
            .map(|(_, key, value)| (key.as_str(), value.clone()))
            .collect();
        assert_eq!(
            changes,
            [
                ("vision.movie.fps", json!(30)),
                ("vision.movie.encode_format", json!(1)),
                ("vision.movie.toolbar_display_type", json!(2)),
                ("vision.movie.exit_record_after_encode", json!(true)),
            ]
        );
        for key in source.as_object().unwrap().keys() {
            assert!(mapper.consumed.contains(&format!("/{key}/v")));
            assert!(mapper.consumed.contains(&format!("/{key}/t")));
        }
    }
}
