use super::Mapper;
use anyhow::{Result, ensure};
use orbment_core::i18n::tr;
use serde_json::{Map, Value, json};
use windows::Win32::Globalization::{CP_ACP, MB_ERR_INVALID_CHARS, MultiByteToWideChar};

pub(super) fn decode_ini(bytes: &[u8]) -> Result<String> {
    if bytes.is_empty() {
        return Ok(String::new());
    }
    if bytes.starts_with(&[0xef, 0xbb, 0xbf])
        || bytes.starts_with(&[0xff, 0xfe])
        || bytes.starts_with(&[0xfe, 0xff])
    {
        return crate::source::decode_text(bytes);
    }
    // Legacy INI files without a BOM use the Windows ANSI codepage.
    let count = unsafe { MultiByteToWideChar(CP_ACP, MB_ERR_INVALID_CHARS, bytes, None) };
    ensure!(count > 0, "{}: ANSI", tr("importer.invalid_source"));
    let mut output = vec![0_u16; count as usize];
    let written =
        unsafe { MultiByteToWideChar(CP_ACP, MB_ERR_INVALID_CHARS, bytes, Some(&mut output)) };
    ensure!(written == count, "{}: ANSI", tr("importer.invalid_source"));
    Ok(String::from_utf16(&output)?)
}

pub(super) fn parse_ini(text: &str) -> Result<Value> {
    let mut output = Map::new();
    let mut section = String::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with([';', '#']) {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|v| v.strip_suffix(']')) {
            section = name.to_owned();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            anyhow::bail!("{}: INI", tr("importer.invalid_source"));
        };
        ensure!(
            !key.trim().is_empty(),
            "{}: INI",
            tr("importer.invalid_source")
        );
        let table = output.entry(section.clone()).or_insert_with(|| json!({}));
        table[key.trim()] = json!(value.trim().trim_matches('"'));
    }
    Ok(Value::Object(output))
}
fn converted(m: &mut Mapper<'_>, source: &str, target: &str, kind: &str) {
    let Some(value) = m.get(source).and_then(Value::as_str) else {
        return;
    };
    let value = match kind {
        "bool" => match value {
            "1" | "true" => Some(json!(true)),
            "0" | "false" => Some(json!(false)),
            _ => None,
        },
        "int" => value.parse::<i64>().ok().map(Value::from),
        "float" => value
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map(Value::from),
        "list" => value
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| v.parse::<u32>().ok())
            .collect::<Option<Vec<_>>>()
            .map(|v| json!(v)),
        _ => Some(json!(value)),
    };
    if let Some(value) = value {
        m.put(source, target, value);
    }
}
pub(super) fn map(m: &mut Mapper<'_>) {
    for (source, target, kind) in [
        ("general/monitor_time_span", "sample_interval_ms", "int"),
        (
            "general/get_cpu_usage_by_cpu_times",
            "cpu_usage_by_times",
            "bool",
        ),
        ("general/hard_disk_name", "hard_disk_name", "text"),
        ("general/cpu_core_name", "cpu_core_name", "text"),
        ("connection/auto_select", "adapter_auto_select", "bool"),
        ("connection/select_all", "adapter_select_all", "bool"),
        (
            "connection/connection_name",
            "selected_adapter_name",
            "text",
        ),
        ("config/transparency", "floating.opacity_pct", "int"),
        ("config/always_on_top", "floating.always_on_top", "bool"),
        ("config/lock_window_pos", "floating.lock_position", "bool"),
        ("config/mouse_penetrate", "floating.mouse_penetrate", "bool"),
        ("config/position_x", "floating.pos_x", "int"),
        ("config/position_y", "floating.pos_y", "int"),
        (
            "config/hide_main_wnd_when_fullscreen",
            "floating.hide_on_fullscreen",
            "bool",
        ),
        (
            "config/alow_out_of_border",
            "floating.allow_out_of_border",
            "int",
        ),
        ("config/show_cpu_memory", "floating.show_cpu_memory", "bool"),
        ("config/swap_up_down", "floating.swap_up_down", "bool"),
        ("config/font_name", "floating.font_name", "text"),
        ("config/font_size", "floating.size", "float"),
        ("config/font_style", "floating.style", "int"),
        ("config/text_color", "floating.text_colors", "list"),
        (
            "config/specify_each_item_color",
            "floating.specify_each_item_color",
            "bool",
        ),
        ("config/skin_selected", "floating.skin_selected", "text"),
        (
            "config/allow_skin_cover_font",
            "floating.allow_skin_cover_font",
            "bool",
        ),
        (
            "config/allow_skin_cover_text",
            "floating.allow_skin_cover_text",
            "bool",
        ),
        ("config/window_snap", "floating.snap", "bool"),
        ("config/show_task_bar_wnd", "taskbar.enabled", "bool"),
        ("task_bar/font_name", "taskbar.font_name", "text"),
        ("task_bar/font_size", "taskbar.size", "float"),
        ("task_bar/font_style", "taskbar.style", "int"),
        ("task_bar/task_bar_back_color", "taskbar.back_color", "int"),
        (
            "task_bar/transparent_color",
            "taskbar.transparent_color",
            "int",
        ),
        (
            "task_bar/status_bar_color",
            "taskbar.status_bar_color",
            "int",
        ),
        (
            "task_bar/task_bar_text_color",
            "taskbar.text_colors",
            "list",
        ),
        (
            "task_bar/specify_each_item_color",
            "taskbar.specify_each_item_color",
            "bool",
        ),
        ("task_bar/task_bar_wnd_on_left", "taskbar.on_left", "bool"),
        ("task_bar/task_bar_wnd_snap", "taskbar.snap", "bool"),
        ("config/memory_display", "memory_display_mode", "int"),
        ("config/double_click_action", "double_click_action", "int"),
        ("config/double_click_exe", "double_click_exe", "text"),
        ("config/unit_byte", "unit_byte", "bool"),
        ("config/speed_unit", "speed_unit", "int"),
        ("config/speed_short_mode", "speed_short_mode", "bool"),
        (
            "config/separate_value_unit_with_space",
            "separate_value_unit_with_space",
            "bool",
        ),
        ("config/hide_unit", "hide_unit", "bool"),
        ("config/hide_percent", "hide_percent", "bool"),
        (
            "notify_tip/traffic_tip_enable",
            "alerts.traffic_enabled",
            "bool",
        ),
        (
            "notify_tip/traffic_tip_value",
            "alerts.traffic_value",
            "int",
        ),
        (
            "notify_tip/memory_usage_tip_enable",
            "alerts.memory_enabled",
            "bool",
        ),
        ("notify_tip/memory_tip_value", "alerts.memory_pct", "int"),
        (
            "notify_tip/cpu_temperature_tip_enable",
            "alerts.cpu_temperature_enabled",
            "bool",
        ),
        (
            "notify_tip/cpu_temperature_tip_value",
            "alerts.cpu_temperature_c",
            "float",
        ),
        (
            "notify_tip/gpu_temperature_tip_enable",
            "alerts.gpu_temperature_enabled",
            "bool",
        ),
        (
            "notify_tip/gpu_temperature_tip_value",
            "alerts.gpu_temperature_c",
            "float",
        ),
        (
            "notify_tip/hdd_temperature_tip_enable",
            "alerts.disk_temperature_enabled",
            "bool",
        ),
        (
            "notify_tip/hdd_temperature_tip_value",
            "alerts.disk_temperature_c",
            "float",
        ),
    ] {
        converted(m, &format!("/{source}"), &format!("monitor.{target}"), kind);
    }
    for (source, target, bit) in [
        (
            "/general/hardware_monitor_item",
            "monitor.hardware_monitor_mask",
            8,
        ),
        (
            "/task_bar/tbar_display_item",
            "monitor.taskbar.display_mask",
            256,
        ),
    ] {
        if let Some(mask) = m
            .get(source)
            .and_then(Value::as_str)
            .and_then(|v| v.parse::<u32>().ok())
        {
            m.put(source, target, json!(mask & !bit));
        }
    }
    if let Some(hidden) = m.get("/config/hide_main_window").and_then(Value::as_str)
        && matches!(hidden, "0" | "1" | "false" | "true")
    {
        m.put(
            "/config/hide_main_window",
            "monitor.floating.enabled",
            json!(matches!(hidden, "0" | "false")),
        );
    }
    if let Some(method) = m
        .get("/general/cpu_usage_acquire_method")
        .and_then(Value::as_str)
        && matches!(method, "0" | "1")
    {
        m.put(
            "/general/cpu_usage_acquire_method",
            "monitor.cpu_usage_by_times",
            json!(method == "0"),
        );
    }
    m.choice(
        "/notify_tip/traffic_tip_unit",
        "monitor.alerts.traffic_unit",
        &[("0", "MB"), ("1", "GB")],
    );
    if let Some(value) = m.get("/general/connections_hide").and_then(Value::as_str) {
        let mut names: Vec<String> = if value.is_empty() {
            Vec::new()
        } else {
            value
                .split("\",\"")
                .map(|name| name.trim_matches('"').to_owned())
                .collect()
        };
        names.sort();
        names.dedup();
        m.put(
            "/general/connections_hide",
            "monitor.hidden_adapters",
            json!(names),
        );
    }
    for (source, field) in [
        ("up", "up"),
        ("down", "down"),
        ("cpu", "cpu"),
        ("memory", "memory"),
        ("gpu", "gpu"),
        ("cpu_temp", "cpu_temperature"),
        ("gpu_temp", "gpu_temperature"),
        ("hdd_temp", "disk_temperature"),
        ("hdd", "disk"),
        ("cpu_freq", "cpu_frequency"),
        ("total_speed", "total_speed"),
        ("today_traffic", "today_traffic"),
    ] {
        m.copy(
            &format!("/config/{source}_string"),
            &format!("monitor.labels.{field}"),
        );
        m.copy(
            &format!("/task_bar/{source}_string"),
            &format!("monitor.taskbar.labels.{field}"),
        );
    }
    // Mainboard-temperature settings deliberately remain unmapped; this module has no sensor.
}
