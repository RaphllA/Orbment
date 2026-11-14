use super::Mapper;
use orbment_core::config::{AppSoundRule, AudioDeviceSelection, AudioProfile};
use serde_json::{Value, json};

pub(super) fn map(m: &mut Mapper<'_>) {
    m.pairs(&[
        ("/ChangeCommunications", "audio.change_communications"),
        (
            "/SwitchForegroundProgram",
            "audio.switch_foreground_program",
        ),
        ("/KeepVolumeEnabled", "audio.keep_volume_enabled"),
        ("/AutoAddNewConnectedDevices", "audio.auto_add_new_devices"),
        ("/QuickMenuEnabled", "audio.quick_menu_enabled"),
        ("/BannerOpacityPercentage", "audio.banner_opacity_pct"),
        ("/MaxNumberNotification", "audio.max_number_notifications"),
        (
            "/NotifyUsingPrimaryScreen",
            "audio.notify_using_primary_screen",
        ),
    ]);
    m.choice(
        "/CyclerType",
        "audio.cycle_mode",
        &[
            ("0", "All"),
            ("1", "Available"),
            ("All", "All"),
            ("Available", "Available"),
        ],
    );
    m.choice(
        "/IconDoubleClick",
        "audio.icon_double_click",
        &[
            ("0", "SwitchDevice"),
            ("1", "OpenSettings"),
            ("SwitchDevice", "SwitchDevice"),
            ("OpenSettings", "OpenSettings"),
        ],
    );
    m.choice(
        "/TooltipInfo",
        "audio.tooltip_info",
        &[
            ("0", "Recording"),
            ("1", "Playback"),
            ("2", "Both"),
            ("3", "None"),
            ("Playback", "Playback"),
            ("Recording", "Recording"),
            ("Both", "Both"),
            ("None", "None"),
        ],
    );
    m.choice(
        "/SwitchIcon",
        "audio.switch_icon_mode",
        &[
            ("0", "Never"),
            ("1", "Recording"),
            ("2", "Playback"),
            ("Playback", "Playback"),
            ("Recording", "Recording"),
            ("Both", "Both"),
            ("Never", "Never"),
        ],
    );
    m.choice(
        "/BannerDisplayInfo",
        "audio.banner_display_info",
        &[
            ("0", "Full"),
            ("1", "IconOnly"),
            ("2", "TextOnly"),
            ("FullDisplay", "Full"),
            ("Full", "Full"),
            ("IconOnly", "IconOnly"),
            ("DescriptionOnly", "TextOnly"),
            ("TextOnly", "TextOnly"),
        ],
    );
    m.choice(
        "/BannerPosition",
        "audio.banner_position",
        &[
            ("0", "TopLeft"),
            ("1", "TopCenter"),
            ("2", "TopRight"),
            ("3", "BottomLeft"),
            ("4", "BottomCenter"),
            ("5", "BottomRight"),
            ("6", "MiddleCenter"),
            ("7", "MiddleLeft"),
            ("8", "MiddleRight"),
            ("9", "Custom"),
            ("TopLeft", "TopLeft"),
            ("TopCenter", "TopCenter"),
            ("TopRight", "TopRight"),
            ("BottomLeft", "BottomLeft"),
            ("BottomCenter", "BottomCenter"),
            ("BottomRight", "BottomRight"),
            ("Center", "MiddleCenter"),
            ("CenterLeft", "MiddleLeft"),
            ("CenterRight", "MiddleRight"),
            ("Custom", "Custom"),
        ],
    );
    if let (Some(x), Some(y)) = (
        m.get("/CustomBannerPosition/X").and_then(Value::as_i64),
        m.get("/CustomBannerPosition/Y").and_then(Value::as_i64),
    ) {
        m.put(
            "/CustomBannerPosition",
            "audio.custom_banner_position",
            json!([x, y]),
        );
    }
    if let Some((x, y)) = m
        .get("/CustomBannerPosition")
        .and_then(Value::as_str)
        .and_then(|v| v.split_once(','))
        && let (Ok(x), Ok(y)) = (x.trim().parse::<i32>(), y.trim().parse::<i32>())
    {
        m.put(
            "/CustomBannerPosition",
            "audio.custom_banner_position",
            json!([x, y]),
        );
    }
    for (source, target) in [
        ("SwitchDeviceNotification", "switch_device_notification"),
        ("SwitchProfileNotification", "switch_profile_notification"),
        ("MicrophoneMuteNotification", "mic_mute_notification"),
    ] {
        m.choice(
            &format!("/{source}"),
            &format!("audio.{target}"),
            &[
                ("0", "SystemToast"),
                ("1", "Sound"),
                ("2", "None"),
                ("4", "SystemToast"),
                ("5", "Banner"),
                ("DefaultWindowsNotification", "SystemToast"),
                ("SoundNotification", "Sound"),
                ("NoNotification", "None"),
                ("ToastNotification", "SystemToast"),
                ("BannerNotification", "Banner"),
                ("Banner", "Banner"),
                ("None", "None"),
                ("SystemToast", "SystemToast"),
                ("Sound", "Sound"),
            ],
        );
    }
    for (source, target) in [
        ("MicrophoneMuteBanner", "mic_mute_banner_mode"),
        ("MicrophoneUnmuteBanner", "mic_unmute_banner_mode"),
        ("UnmuteBanner", "mic_unmute_banner_mode"),
    ] {
        m.choice(
            &format!("/{source}"),
            &format!("audio.{target}"),
            &[
                ("0", "None"),
                ("1", "Temporary"),
                ("2", "Persistent"),
                ("None", "None"),
                ("Fading", "Temporary"),
                ("Temporary", "Temporary"),
                ("Persistent", "Persistent"),
            ],
        );
    }
    for (source, target) in [
        ("PlaybackHotKey", "playback_hotkey"),
        ("RecordingHotKey", "recording_hotkey"),
        ("MuteRecordingHotKey", "mute_recording_hotkey"),
    ] {
        if let Some(key) = m.get(&format!("/{source}")).and_then(hotkey) {
            m.put(
                &format!("/{source}"),
                &format!("audio.{target}"),
                json!(key),
            );
        }
    }
    if let Some(ms) = m
        .get("/BannerOnScreenTime")
        .and_then(Value::as_str)
        .and_then(timespan_ms)
    {
        m.put(
            "/BannerOnScreenTime",
            "audio.banner_on_screen_time_ms",
            json!(ms),
        );
    }
    if let Some(devices) = m.get("/SelectedDevices").and_then(Value::as_array) {
        let mut playback = Vec::new();
        let mut recording = Vec::new();
        let mut valid = true;
        for device in devices {
            let (Some(id), Some(name), Some(kind)) = (
                device["Id"].as_str(),
                device["NameClean"]
                    .as_str()
                    .or_else(|| device["Name"].as_str()),
                device["Type"].as_u64(),
            ) else {
                valid = false;
                break;
            };
            let device = AudioDeviceSelection {
                id: id.to_owned(),
                name: name.to_owned(),
            };
            match kind {
                0 => playback.push(device),
                1 => recording.push(device),
                _ => {
                    valid = false;
                    break;
                }
            }
        }
        if valid {
            m.put(
                "/SelectedDevices",
                "audio.selected_playback_devices",
                json!(playback),
            );
            m.put(
                "/SelectedDevices",
                "audio.selected_recording_devices",
                json!(recording),
            );
        }
    }
    if let Some(profiles) = m.get("/Profiles").and_then(Value::as_array)
        && let Some(profiles) = profiles
            .iter()
            .enumerate()
            .map(|(index, value)| profile(value, index))
            .collect::<Option<Vec<_>>>()
    {
        m.put("/Profiles", "audio.profiles", json!(profiles));
    }
    if let Some(rules) = m.get("/AppSoundRules").and_then(Value::as_array) {
        let rules = rules
            .iter()
            .map(|value| {
                // Title matching and case-sensitive rules are not representable by the runtime.
                if value["WindowName"].as_str().is_some_and(|v| !v.is_empty())
                    || value["CaseSensitive"].as_bool() == Some(true)
                {
                    return None;
                }
                Some(AppSoundRule {
                    process_path: value["ProcessPath"].as_str()?.to_owned(),
                    playback_device_id: value
                        .pointer("/PlaybackDevice/Id")
                        .and_then(Value::as_str)
                        .or_else(|| value["PlaybackDeviceId"].as_str())
                        .map(str::to_owned),
                    recording_device_id: value
                        .pointer("/RecordingDevice/Id")
                        .and_then(Value::as_str)
                        .or_else(|| value["RecordingDeviceId"].as_str())
                        .map(str::to_owned),
                    enabled: value["Enabled"].as_bool().unwrap_or(true),
                })
            })
            .collect::<Option<Vec<_>>>();
        if let Some(rules) = rules {
            m.put("/AppSoundRules", "audio.app_sound_rules", json!(rules));
        }
    }
}

pub(super) fn hotkey(value: &Value) -> Option<String> {
    if value["Enabled"].as_bool() == Some(false) {
        return Some(String::new());
    }
    let modifier = value["Modifier"].as_u64()?;
    if modifier & !15 != 0 {
        return None;
    }
    let key = value["Keys"].as_u64()?;
    let name = match key {
        65..=90 | 48..=57 => char::from_u32(key as u32)?.to_string(),
        112..=135 => format!("F{}", key - 111),
        8 => "Backspace".into(),
        9 => "Tab".into(),
        13 => "Enter".into(),
        27 => "Escape".into(),
        32 => "Space".into(),
        33 => "PageUp".into(),
        34 => "PageDown".into(),
        35 => "End".into(),
        36 => "Home".into(),
        37 => "Left".into(),
        38 => "Up".into(),
        39 => "Right".into(),
        40 => "Down".into(),
        45 => "Insert".into(),
        46 => "Delete".into(),
        _ => return None,
    };
    let mut parts = Vec::new();
    for (bit, label) in [(2, "Ctrl"), (4, "Shift"), (1, "Alt"), (8, "Win")] {
        if modifier & bit != 0 {
            parts.push(label.to_owned());
        }
    }
    parts.push(name);
    Some(parts.join("+"))
}
fn timespan_ms(text: &str) -> Option<u32> {
    let parts: Vec<_> = text.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let hours: u64 = parts[0].parse().ok()?;
    let minutes: u64 = parts[1].parse().ok()?;
    let seconds: f64 = parts[2].parse().ok()?;
    if minutes >= 60 || !(0.0..60.0).contains(&seconds) {
        return None;
    }
    let milliseconds =
        (hours.checked_mul(3600)?.checked_add(minutes * 60)? as f64 + seconds) * 1000.0;
    if milliseconds > f64::from(u32::MAX) || milliseconds.fract() != 0.0 {
        None
    } else {
        Some(milliseconds as u32)
    }
}
fn profile(value: &Value, index: usize) -> Option<AudioProfile> {
    if value["RestoreDevices"].as_bool() == Some(true)
        || value["SwitchForegroundApp"].as_bool() == Some(true)
        || value["AlsoSwitchDefaultDevice"].as_bool() == Some(false)
    {
        return None;
    }
    let playback = value.pointer("/Playback/Id").and_then(Value::as_str);
    let recording = value.pointer("/Recording/Id").and_then(Value::as_str);
    let communication = value.pointer("/Communication/Id").and_then(Value::as_str);
    let recording_communication = value
        .pointer("/RecordingCommunication/Id")
        .and_then(Value::as_str);
    if communication.is_some_and(|id| Some(id) != playback)
        || recording_communication.is_some_and(|id| Some(id) != recording)
    {
        return None;
    }
    let mut profile = AudioProfile {
        id: value["Id"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("audio-profile-{index}")),
        name: value["Name"].as_str()?.to_owned(),
        playback_device_id: playback.map(str::to_owned),
        recording_device_id: recording.map(str::to_owned),
        change_communications: communication.is_some() || recording_communication.is_some(),
        ..AudioProfile::default()
    };
    for trigger in value["Triggers"].as_array()? {
        if trigger["ShouldRestoreDevices"].as_bool() == Some(true) {
            return None;
        }
        match trigger["Type"].as_u64()? {
            0 => {
                if profile.hotkey.is_some() {
                    return None;
                }
                profile.hotkey = Some(hotkey(&trigger["HotKey"])?);
            }
            1 | 2 => {
                if trigger["WindowName"]
                    .as_str()
                    .is_some_and(|v| !v.is_empty())
                {
                    return None;
                }
                let path = trigger["ApplicationPath"].as_str()?;
                if profile.process_path.as_deref().is_some_and(|v| v != path) {
                    return None;
                }
                profile.process_path = Some(path.to_owned());
                profile.on_window_activate = true;
            }
            6 => {}
            _ => return None,
        }
    }
    Some(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn app_sound_rules_prefer_nested_device_ids_with_legacy_fallback() {
        let source = json!({
            "AppSoundRules": [
                {
                    "ProcessPath": "C:\\Apps\\modern.exe",
                    "PlaybackDevice": { "Id": "nested-render" },
                    "PlaybackDeviceId": "legacy-render",
                    "RecordingDevice": { "Id": "nested-capture" },
                    "RecordingDeviceId": "legacy-capture",
                    "Enabled": true
                },
                {
                    "ProcessPath": "C:\\Apps\\legacy.exe",
                    "PlaybackDeviceId": "legacy-only-render",
                    "RecordingDeviceId": "legacy-only-capture",
                    "Enabled": false
                }
            ]
        });
        let mut mapper = Mapper {
            source: &source,
            changes: Vec::new(),
            consumed: BTreeSet::new(),
        };
        map(&mut mapper);

        let (_, key, value) = mapper
            .changes
            .into_iter()
            .find(|(_, key, _)| key == "audio.app_sound_rules")
            .expect("app_sound_rules should be mapped");
        assert_eq!(key, "audio.app_sound_rules");
        let rules: Vec<AppSoundRule> = serde_json::from_value(value).unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].process_path, "C:\\Apps\\modern.exe");
        assert_eq!(
            rules[0].playback_device_id.as_deref(),
            Some("nested-render")
        );
        assert_eq!(
            rules[0].recording_device_id.as_deref(),
            Some("nested-capture")
        );
        assert!(rules[0].enabled);
        assert_eq!(rules[1].process_path, "C:\\Apps\\legacy.exe");
        assert_eq!(
            rules[1].playback_device_id.as_deref(),
            Some("legacy-only-render")
        );
        assert_eq!(
            rules[1].recording_device_id.as_deref(),
            Some("legacy-only-capture")
        );
        assert!(!rules[1].enabled);
    }

    #[test]
    fn profiles_use_neutral_fallback_ids_and_preserve_explicit_ids() {
        let mut source = json!({
            "Name": "Default",
            "Triggers": []
        });
        assert_eq!(profile(&source, 7).unwrap().id, "audio-profile-7");
        source["Id"] = json!("custom-profile");
        assert_eq!(profile(&source, 7).unwrap().id, "custom-profile");
    }

    #[test]
    fn profile_triggers_map_type_2_and_windowless_type_1_to_window_activate() {
        let process_trigger = json!({
            "Name": "Game",
            "Playback": { "Id": "render-1" },
            "Triggers": [
                {
                    "Type": 2,
                    "ApplicationPath": "C:\\Games\\game.exe",
                    "WindowName": ""
                }
            ]
        });
        let mapped = profile(&process_trigger, 0).expect("Type=2 trigger should map");
        assert_eq!(mapped.process_path.as_deref(), Some("C:\\Games\\game.exe"));
        assert!(mapped.on_window_activate);
        assert!(!mapped.on_process_start);

        let window_trigger_without_title = json!({
            "Name": "Player",
            "Playback": { "Id": "render-1" },
            "Triggers": [
                {
                    "Type": 1,
                    "ApplicationPath": "C:\\Apps\\player.exe",
                    "WindowName": ""
                }
            ]
        });
        let mapped_type_1 = profile(&window_trigger_without_title, 1)
            .expect("Windowless Type=1 trigger should map");
        assert_eq!(
            mapped_type_1.process_path.as_deref(),
            Some("C:\\Apps\\player.exe")
        );
        assert!(mapped_type_1.on_window_activate);
        assert!(!mapped_type_1.on_process_start);

        let window_trigger_with_title = json!({
            "Name": "Editor",
            "Playback": { "Id": "render-1" },
            "Triggers": [
                {
                    "Type": 1,
                    "ApplicationPath": "C:\\Apps\\editor.exe",
                    "WindowName": "Document - Editor"
                }
            ]
        });
        assert!(profile(&window_trigger_with_title, 2).is_none());
    }
}
