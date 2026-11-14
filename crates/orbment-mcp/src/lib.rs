use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::io::{self, BufRead, Write};

const PROTOCOL_VERSION: &str = "2024-11-05";

struct Tool {
    action: &'static str,
    description: &'static str,
    schema: fn() -> Value,
}

fn empty_schema() -> Value {
    object_schema(json!({}), &[])
}

fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

// Keep canonical dotted names here: reverse mapping must preserve underscores in actions.
const TOOLS: &[Tool] = &[
    Tool {
        action: "app.status",
        description: "Read module lifecycle states, process diagnostics and database path.",
        schema: empty_schema,
    },
    Tool {
        action: "app.config_get",
        description: "Read configuration at a dotted path; omit path to read all configuration.",
        schema: || object_schema(json!({"path":{"type":"string"}}), &[]),
    },
    Tool {
        action: "app.config_set",
        description: "Update a configuration value at a dotted path. The server rejects protected settings.",
        schema: || {
            object_schema(
                json!({"path":{"type":"string"},"value":{}}),
                &["path", "value"],
            )
        },
    },
    Tool {
        action: "audio.list_devices",
        description: "List playback and recording devices, default states and volume.",
        schema: empty_schema,
    },
    Tool {
        action: "audio.switch_playback",
        description: "Switch to a playback device ID, or cycle selected devices if omitted.",
        schema: device_schema,
    },
    Tool {
        action: "audio.switch_recording",
        description: "Switch to a recording device ID, or cycle selected devices if omitted.",
        schema: device_schema,
    },
    Tool {
        action: "audio.route_process",
        description: "Route a process to an audio device; omit the process selector for the foreground process.",
        schema: || {
            object_schema(
                json!({
                    "device_id":{"type":"string"},
                    "pid":{"type":"integer","minimum":1,"maximum":4294967295_u64},
                    "process_path":{"type":"string","description":"Full executable path."},
                    "process_name":{"type":"string","description":"Executable basename; routes all matching processes."},
                    "recording":{"type":"boolean","default":false}
                }),
                &["device_id"],
            )
        },
    },
    Tool {
        action: "audio.toggle_mic_mute",
        description: "Toggle the default recording device's microphone mute state.",
        schema: empty_schema,
    },
    Tool {
        action: "monitor.snapshot",
        description: "Read current network, CPU, memory, GPU, disk and hardware temperature samples.",
        schema: empty_schema,
    },
    Tool {
        action: "monitor.traffic_history",
        description: "Read daily network traffic totals between optional inclusive dates.",
        schema: || {
            object_schema(
                json!({
                    "start_date":{"type":"string","format":"date"},
                    "end_date":{"type":"string","format":"date"}
                }),
                &[],
            )
        },
    },
    Tool {
        action: "memory.overview",
        description: "Read physical and virtual memory, system working sets and the highest-memory processes.",
        schema: || {
            object_schema(
                json!({"top_n":{"type":"integer","minimum":1,"maximum":1000,"default":20}}),
                &[],
            )
        },
    },
    Tool {
        action: "memory.clean",
        description: "Clean selected memory regions and report released bytes. Requires allow_memory_clean.",
        schema: || {
            object_schema(
                json!({
                    "regions_mask":{"type":"integer","minimum":1,"maximum":63,"description":"Bitmask: working set=1, system working set=2, standby priority 0=4, standby=8, modified=16, combine=32. Omit to use configured regions."},
                    "confirmed":{"type":"boolean","default":false,"description":"Acknowledge configured cleanup confirmation and standby/modified-page stall warnings."}
                }),
                &[],
            )
        },
    },
    Tool {
        action: "memory.port_lookup",
        description: "Find TCP/UDP port owners, including PID, executable path, command line and process creation time. Filters intersect; omit both to list all.",
        schema: || {
            object_schema(
                json!({
                    "port":{"type":"integer","minimum":1,"maximum":65535},
                    "process_name":{"type":"string"}
                }),
                &[],
            )
        },
    },
    Tool {
        action: "memory.process_kill",
        description: "Terminate a confirmed process identity obtained from a read query. Requires allow_process_kill.",
        schema: || {
            object_schema(
                json!({
                    "pid":{"type":"integer","minimum":1,"maximum":4294967295_u64},
                    "creation_time":{"type":"string","pattern":"^[0-9]+$","description":"Exact decimal Windows FILETIME creation_time string returned by a read query; preserve it unchanged to reject PID reuse without numeric precision loss."},
                    "confirmed":{"type":"boolean","const":true,"description":"Explicit confirmation to terminate this process."}
                }),
                &["pid", "creation_time", "confirmed"],
            )
        },
    },
    Tool {
        action: "memory.process_reap_preview",
        description: "Read suspected leftover process families. Retain the returned preview and process identities for an explicitly confirmed reap.",
        schema: empty_schema,
    },
    Tool {
        action: "memory.process_reap",
        description: "Close selected leftover processes from a preview after revalidation. Force termination requires separate confirmation. Requires allow_process_reap.",
        schema: || {
            object_schema(
                json!({
                    "preview":{"type":"object","description":"The complete, unchanged object returned by memory_process_reap_preview."},
                    "targets":{"type":"array","minItems":1,"uniqueItems":true,"items":process_identity_schema(),"description":"Selected process identities from the supplied preview."},
                    "mode":{"type":"string","enum":["Graceful","Force"],"description":"Graceful requests window closure; Force terminates the selected processes."},
                    "confirmed":{"type":"boolean","const":true},
                    "force_confirmed":{"type":"boolean","default":false,"description":"Must also be true when mode is Force."}
                }),
                &["preview", "targets", "mode", "confirmed"],
            )
        },
    },
    Tool {
        action: "utilities.set_topmost",
        description: "Set or clear a window's topmost state; omit enabled to toggle. Select by handle, title or target; omit selectors to use the configured target.",
        schema: || {
            object_schema(
                json!({
                    "hwnd":{"type":"integer","minimum":-9223372036854775808_i64,"maximum":9223372036854775807_i64,"not":{"const":0},"description":"Nonzero signed window handle; do not combine with title or target."},
                    "title":{"type":"string","description":"Window title; do not combine with hwnd or target."},
                    "target":{"type":"string","enum":["Foreground","UnderCursor"]},
                    "enabled":{"type":"boolean","description":"True sets topmost; false clears it; omit to toggle."}
                }),
                &[],
            )
        },
    },
    Tool {
        action: "utilities.pick_color",
        description: "Open the screen color picker, or supply both x and y to sample and copy a color immediately.",
        schema: || {
            object_schema(
                json!({
                    "format":{"type":"string","enum":["Hex","Rgb","Hsl","Hsb","Cmyk","Oklab","Oklch","Lab","Bgr","Custom"],"description":"Omit to use the configured color format. Custom uses utilities.color_template."},
                    "x":{"type":"integer","minimum":-2147483648_i64,"maximum":2147483647,"description":"Screen pixel x coordinate; requires y. Negative coordinates support secondary monitors."},
                    "y":{"type":"integer","minimum":-2147483648_i64,"maximum":2147483647,"description":"Screen pixel y coordinate; requires x."}
                }),
                &[],
            )
        },
    },
    Tool {
        action: "utilities.keep_awake",
        description: "Prevent system sleep or keep the display on. Omit enabled to toggle; omitted mode and duration use configured defaults.",
        schema: || {
            object_schema(
                json!({
                    "enabled":{"type":"boolean"},
                    "mode":{"type":"string","enum":["System","Display"],"description":"Display also prevents system sleep."},
                    "duration":{"type":"string","enum":["Indefinite","Minutes30","Hour1","Hours2","Process"]},
                    "process_id":{"type":"integer","minimum":1,"maximum":4294967295_u64,"description":"Required for Process duration; release the power request when this process exits."}
                }),
                &[],
            )
        },
    },
    Tool {
        action: "utilities.display_brightness",
        description: "Read hardware brightness, set one display's percent, or adjust all displays by delta. Use current inventory IDs; hotplug invalidates old IDs.",
        schema: || {
            object_schema(
                json!({
                    "id":{"type":"integer","minimum":1,"maximum":18446744073709551615_u64,"description":"Display ID from the returned inventory; required with percent."},
                    "percent":{"type":"integer","minimum":0,"maximum":100,"description":"Absolute brightness for id; do not combine with delta."},
                    "delta":{"type":"integer","minimum":-100,"maximum":100,"description":"Relative brightness adjustment for all displays; do not combine with id or percent."},
                    "refresh":{"type":"boolean","default":false,"description":"Refresh monitor discovery before returning the snapshot."}
                }),
                &[],
            )
        },
    },
    Tool {
        action: "clipboard.write",
        description: "Write exactly one of text, an image file, or a file list to the Windows clipboard. Internal writes are excluded from history capture.",
        schema: || {
            object_schema(
                json!({
                    "text":{"type":"string"},
                    "image_path":{"type":"string"},
                    "files":{"type":"array","items":{"type":"string"},"minItems":1}
                }),
                &[],
            )
        },
    },
    Tool {
        action: "clipboard.paste",
        description: "Paste a history item by integer id or supplied text, image_path or files into the foreground window. Content selectors are mutually exclusive; omit all to paste the most recent item.",
        schema: || {
            object_schema(
                json!({
                    "id":{"type":"integer","minimum":1},
                    "text":{"type":"string"},
                    "image_path":{"type":"string"},
                    "files":{"type":"array","items":{"type":"string"},"minItems":1},
                    "plain_text":{"type":"boolean","default":false}
                }),
                &[],
            )
        },
    },
    Tool {
        action: "file_search.query",
        description: "按查询条件搜索文件，可指定返回结果数量上限。",
        schema: || {
            object_schema(
                json!({
                    "query":{"type":"string"},
                    "limit":{"type":"integer","minimum":0}
                }),
                &["query"],
            )
        },
    },
    Tool {
        action: "disk_analyzer.top_consumers",
        description: "查询指定路径下占用空间最多的项目，可限制结果数量并选择按实际分配空间统计。",
        schema: || {
            object_schema(
                json!({
                    "path":{"type":"string"},
                    "limit":{"type":"integer","minimum":0},
                    "allocated":{"type":"boolean"}
                }),
                &["path"],
            )
        },
    },
    Tool {
        action: "disk_analyzer.file_lock_query",
        description: "查询占用指定文件路径的进程及文件句柄。",
        schema: || object_schema(json!({"path":{"type":"string"}}), &["path"]),
    },
    Tool {
        action: "disk_analyzer.file_unlock",
        description: "使用 file_lock_query 返回的进程创建时间和文件路径解除占用；指定 handle 关闭该句柄，否则结束进程。需要启用 mcp.allow_file_unlock，强制解锁可能导致数据丢失或进程异常。",
        schema: || {
            object_schema(
                json!({
                    "process_id":{"type":"integer","minimum":1,"maximum":4294967295_u64},
                    "creation_time":{"type":"string","pattern":"^[0-9]+$","description":"Preserve the exact creation_time returned by file_lock_query to reject PID reuse."},
                    "path":{"type":"string","minLength":1,"description":"Preserve the target file path returned by file_lock_query."},
                    "handle":{"type":"integer","minimum":0,"maximum":18446744073709551615_u64}
                }),
                &["process_id", "creation_time", "path"],
            )
        },
    },
    Tool {
        action: "vision.capture_screen",
        description: "Capture a monitor, window or physical-pixel desktop rectangle; omit selectors for the virtual desktop. Return the saved image path and optionally create a pin.",
        schema: || {
            object_schema(
                json!({
                    "monitor":{"type":"integer","minimum":0},
                    "window":{"type":"integer"},
                    "region":region_schema(),
                    "output_path":{"type":"string"},
                    "pin":{"type":"boolean","default":false}
                }),
                &[],
            )
        },
    },
    Tool {
        action: "vision.ocr",
        description: "Recognize an image file or physical-pixel screen region and return text with bounding boxes. The configured engine is used when omitted; first-use Ask requires a local choice.",
        schema: || {
            object_schema(
                json!({
                    "image_path":{"type":"string"},
                    "region":region_schema(),
                    "engine":{"type":"string","enum":["WindowsNative","OfflineOnnx"]}
                }),
                &[],
            )
        },
    },
    Tool {
        action: "vision.pin_create",
        description: "Create a desktop pin from exactly one image path, text, HTML, color or live window. Coordinates are virtual-desktop physical pixels.",
        schema: || {
            object_schema(
                json!({
                    "image_path":{"type":"string"},
                    "text":{"type":"string"},
                    "html":{"type":"string"},
                    "color":{"type":"string"},
                    "window":{"type":"integer"},
                    "region":region_schema(),
                    "x":{"type":"number"},
                    "y":{"type":"number"},
                    "group":{"type":"string"}
                }),
                &[],
            )
        },
    },
    Tool {
        action: "vision.pin_list",
        description: "List open desktop pins with their content, placement and grouping metadata.",
        schema: empty_schema,
    },
    Tool {
        action: "vision.pin_close",
        description: "Close one desktop pin and persist its closed state.",
        schema: || object_schema(json!({"id":{"type":"string"}}), &["id"]),
    },
];

fn region_schema() -> Value {
    object_schema(
        json!({
            "x":{"type":"number"},
            "y":{"type":"number"},
            "width":{"type":"number","exclusiveMinimum":0},
            "height":{"type":"number","exclusiveMinimum":0}
        }),
        &["x", "y", "width", "height"],
    )
}

/// The same tool contract feeds MCP and provider-compatible QuickAsk requests.
pub fn tool_definitions() -> Vec<Value> {
    TOOLS
        .iter()
        .map(|tool| {
            json!({
                "name":tool.action.replace('.', "_"),
                "description":tool.description,
                "inputSchema":(tool.schema)()
            })
        })
        .collect()
}

fn device_schema() -> Value {
    object_schema(json!({"device_id":{"type":"string"}}), &[])
}

fn process_identity_schema() -> Value {
    object_schema(
        json!({
            "pid":{"type":"integer","minimum":1,"maximum":4294967295_u64},
            "creation_time":{"type":"string","pattern":"^[0-9]+$","description":"Exact decimal Windows FILETIME creation_time string returned by the preview; preserve it unchanged."}
        }),
        &["pid", "creation_time"],
    )
}

/// Resolve only registered tool names, without treating action underscores as separators.
pub fn tool_to_action(name: &str) -> Option<&'static str> {
    TOOLS
        .iter()
        .find(|tool| tool.action.replace('.', "_") == name)
        .map(|tool| tool.action)
}

/// Check daemon-side MCP/CLI access before dispatching an external action.
/// Protected configuration paths are still checked by `ConfigStore::set_external`.
pub fn check_external_action(config: &orbment_core::config::McpConfig, action: &str) -> Result<()> {
    ensure!(config.enabled, "MCP/CLI access is disabled in settings");
    check_action_risk(config, action)
}

/// Internal model tool calls share risk authorization without enabling an IPC endpoint.
pub fn check_action_risk(config: &orbment_core::config::McpConfig, action: &str) -> Result<()> {
    let allowed = match action {
        "memory.clean" => config.allow_memory_clean,
        "memory.process_kill" => config.allow_process_kill,
        "memory.process_reap" => config.allow_process_reap,
        "disk_analyzer.file_unlock" => config.allow_file_unlock,
        "app.status"
        | "app.config_get"
        | "app.config_set"
        | "audio.list_devices"
        | "audio.switch_playback"
        | "audio.switch_recording"
        | "audio.route_process"
        | "audio.toggle_mic_mute"
        | "monitor.snapshot"
        | "monitor.traffic_history"
        | "memory.overview"
        | "memory.port_lookup"
        | "memory.process_reap_preview"
        | "utilities.set_topmost"
        | "utilities.pick_color"
        | "utilities.keep_awake"
        | "utilities.display_brightness"
        | "clipboard.write"
        | "clipboard.paste"
        | "file_search.query"
        | "disk_analyzer.top_consumers"
        | "disk_analyzer.file_lock_query"
        | "vision.capture_screen"
        | "vision.ocr"
        | "vision.pin_create"
        | "vision.pin_list"
        | "vision.pin_close" => true,
        _ => false,
    };
    ensure!(allowed, "tool action is not permitted: {action}");
    Ok(())
}

/// Serve newline-delimited MCP JSON-RPC on stdio, without starting or elevating the daemon.
/// Stdout contains only protocol messages; transport failures are returned to the caller.
pub fn run_stdio() -> Result<()> {
    serve(
        io::stdin().lock(),
        io::stdout().lock(),
        orbment_core::ipc::request,
    )
}

fn serve(
    mut input: impl BufRead,
    mut output: impl Write,
    mut forward: impl FnMut(&str, Value) -> Result<Value>,
) -> Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        if input.read_line(&mut line).context("read MCP stdin")? == 0 {
            return Ok(());
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(request) => handle(request, &mut forward),
            Err(_) => Some(error(Value::Null, -32700, "Parse error")),
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut output, &response).context("write MCP response")?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn handle(request: Value, forward: &mut impl FnMut(&str, Value) -> Result<Value>) -> Option<Value> {
    let id = request.get("id").cloned();
    let valid_id = id
        .as_ref()
        .is_none_or(|id| id.is_null() || id.is_string() || id.is_number());
    let method = request.get("method").and_then(Value::as_str);
    if request.get("jsonrpc") != Some(&json!("2.0")) || method.is_none() || !valid_id {
        return Some(error(
            if valid_id {
                id.unwrap_or(Value::Null)
            } else {
                Value::Null
            },
            -32600,
            "Invalid Request",
        ));
    }
    // Notifications (including initialized/cancelled) have no response and cannot execute tools.
    let id = id?;
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    if !params.is_object() {
        return Some(error(id, -32602, "Parameters must be an object"));
    }
    let result = match method.unwrap() {
        "initialize" => {
            if !params["protocolVersion"].is_string()
                || !params["capabilities"].is_object()
                || !params["clientInfo"].is_object()
            {
                return Some(error(
                    id,
                    -32602,
                    "Expected protocolVersion, capabilities and clientInfo",
                ));
            }
            json!({
                "protocolVersion":PROTOCOL_VERSION,
                "capabilities":{"tools":{"listChanged":false}},
                "serverInfo":{"name":"orbment","version":env!("CARGO_PKG_VERSION")}
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({"tools":tool_definitions()}),
        "tools/call" => {
            let Some(action) = params["name"].as_str().and_then(tool_to_action) else {
                return Some(error(id, -32602, "Unknown or missing tool name"));
            };
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !arguments.is_object() {
                return Some(error(id, -32602, "Tool arguments must be an object"));
            }
            // Parameter semantics and permissions are validated once, by the daemon.
            match forward(action, arguments) {
                Ok(value) => {
                    json!({"content":[{"type":"text","text":value.to_string()}],"isError":false})
                }
                Err(err) => {
                    json!({"content":[{"type":"text","text":format!("{err:#}")}],"isError":true})
                }
            }
        }
        _ => return Some(error(id, -32601, "Method not found")),
    };
    Some(json!({"jsonrpc":"2.0","id":id,"result":result}))
}

/// Execute `orbment cli <action|diag> [JSON object]` and print the daemon's JSON result.
/// Pass only arguments after `cli`; report returned errors on stderr with a nonzero exit code.
pub fn run_cli(args: &[String]) -> Result<()> {
    let (action, parameters) = parse_cli(args)?;
    let result = orbment_core::ipc::request(action, parameters)?;
    let mut output = io::stdout().lock();
    serde_json::to_writer_pretty(&mut output, &result)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn parse_cli(args: &[String]) -> Result<(&str, Value)> {
    if args.is_empty() || args.len() > 2 {
        bail!("usage: orbment cli <action|diag> [JSON object]");
    }
    let action = if args[0] == "diag" {
        "app.status"
    } else {
        args[0].as_str()
    };
    if !TOOLS.iter().any(|tool| tool.action == action) {
        bail!("unknown action: {action}");
    }
    let parameters = match args.get(1) {
        Some(text) => serde_json::from_str::<Value>(text).context("invalid action JSON")?,
        None => json!({}),
    };
    if !parameters.is_object() {
        bail!("action JSON must be an object");
    }
    Ok((action, parameters))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_mapping_preserves_action_underscores() {
        let definitions = tool_definitions();
        assert_eq!(definitions.len(), TOOLS.len());
        for (definition, tool) in definitions.iter().zip(TOOLS) {
            let name = definition["name"].as_str().unwrap();
            assert_eq!(tool_to_action(name), Some(tool.action));
        }
        assert_eq!(
            tool_to_action("audio_toggle_mic_mute"),
            Some("audio.toggle_mic_mute")
        );
        assert_eq!(tool_to_action("audio.toggle_mic_mute"), None);
        assert!(
            definitions
                .iter()
                .all(|definition| definition["name"] != "memory_unknown")
        );
        assert_eq!(tool_to_action("audio_unknown"), None);
    }

    #[test]
    fn stage3_discovery_and_calls_preserve_action_parameters() {
        let cases = [
            ("memory.overview", json!({"top_n":10})),
            ("memory.clean", json!({"regions_mask":7,"confirmed":true})),
            (
                "memory.port_lookup",
                json!({"port":3000,"process_name":"node.exe"}),
            ),
            (
                "memory.process_kill",
                json!({"pid":42,"creation_time":"134000000000000001","confirmed":true}),
            ),
            ("memory.process_reap_preview", json!({})),
            (
                "memory.process_reap",
                json!({
                    "preview":{"families":[]},
                    "targets":[{"pid":42,"creation_time":"134000000000000001"}],
                    "mode":"Force",
                    "confirmed":true,
                    "force_confirmed":true
                }),
            ),
            (
                "utilities.set_topmost",
                json!({"title":"Editor","enabled":true}),
            ),
            (
                "utilities.pick_color",
                json!({"x":-10,"y":20,"format":"Hex"}),
            ),
            (
                "utilities.keep_awake",
                json!({"enabled":true,"mode":"Display","duration":"Process","process_id":42}),
            ),
            ("utilities.display_brightness", json!({"id":1,"percent":50})),
        ];
        let discovery = handle(
            json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
            &mut |_, _| panic!("discovery must not forward"),
        )
        .unwrap();
        let tools = discovery["result"]["tools"].as_array().unwrap();
        for (action, arguments) in cases {
            let name = action.replace('.', "_");
            let tool = tools.iter().find(|tool| tool["name"] == name).unwrap();
            assert_eq!(tool["inputSchema"]["type"], "object");
            let mut calls = 0;
            let response = handle(
                json!({
                    "jsonrpc":"2.0","id":2,"method":"tools/call",
                    "params":{"name":name,"arguments":arguments}
                }),
                &mut |forwarded_action, forwarded_arguments| {
                    calls += 1;
                    assert_eq!(forwarded_action, action);
                    assert_eq!(forwarded_arguments, arguments);
                    Ok(json!({"accepted":true}))
                },
            )
            .unwrap();
            assert_eq!(calls, 1, "{action}");
            assert_eq!(response["result"]["isError"], false, "{action}");
        }
    }

    #[test]
    fn enabling_external_access_exposes_only_safe_registered_tools_by_default() {
        let config = orbment_core::config::McpConfig::default();
        assert!(check_external_action(&config, "app.status").is_err());
        let config = orbment_core::config::McpConfig {
            enabled: true,
            ..config
        };
        for tool in TOOLS {
            let risky = matches!(
                tool.action,
                "memory.clean"
                    | "memory.process_kill"
                    | "memory.process_reap"
                    | "disk_analyzer.file_unlock"
            );
            assert_eq!(
                check_external_action(&config, tool.action).is_ok(),
                !risky,
                "{}",
                tool.action
            );
        }
        for action in [
            "disk_analyzer.file_unlock",
            "memory.unknown",
            "utilities.unknown",
            "app.quit",
            "memory_process_kill",
            "",
        ] {
            assert!(check_external_action(&config, action).is_err(), "{action}");
        }
    }

    #[test]
    fn external_risk_flags_are_independent() {
        let actions = [
            "memory.clean",
            "memory.process_kill",
            "memory.process_reap",
            "disk_analyzer.file_unlock",
        ];
        for (enabled, _) in actions.iter().enumerate() {
            let config = orbment_core::config::McpConfig {
                enabled: true,
                allow_memory_clean: enabled == 0,
                allow_process_kill: enabled == 1,
                allow_process_reap: enabled == 2,
                allow_file_unlock: enabled == 3,
            };
            for (index, action) in actions.iter().enumerate() {
                assert_eq!(
                    check_external_action(&config, action).is_ok(),
                    index == enabled,
                    "{action}, enabled flag {enabled}"
                );
            }
            assert!(check_external_action(&config, "memory.process_reap_preview").is_ok());
            assert!(check_external_action(&config, "memory.unknown").is_err());
        }
    }

    #[test]
    fn stage5_read_actions_do_not_require_file_unlock_permission() {
        for allow_file_unlock in [false, true] {
            let config = orbment_core::config::McpConfig {
                enabled: true,
                allow_file_unlock,
                ..Default::default()
            };
            for action in [
                "file_search.query",
                "disk_analyzer.top_consumers",
                "disk_analyzer.file_lock_query",
            ] {
                assert!(check_external_action(&config, action).is_ok(), "{action}");
            }
            assert_eq!(
                check_external_action(&config, "disk_analyzer.file_unlock").is_ok(),
                allow_file_unlock
            );
        }
    }

    #[test]
    fn external_master_switch_overrides_all_permissions() {
        let config = orbment_core::config::McpConfig {
            enabled: false,
            allow_memory_clean: true,
            allow_process_kill: true,
            allow_process_reap: true,
            allow_file_unlock: true,
        };
        for action in TOOLS
            .iter()
            .map(|tool| tool.action)
            .chain(["disk_analyzer.file_unlock", "unknown"])
        {
            assert!(check_external_action(&config, action).is_err(), "{action}");
        }
    }

    #[test]
    fn notification_never_forwards_and_call_preserves_arguments() -> Result<()> {
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"params\":{\"name\":\"audio_toggle_mic_mute\"}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":\"call\",\"method\":\"tools/call\",\"params\":{\"name\":\"audio_switch_playback\",\"arguments\":{\"device_id\":\"speaker\"}}}\n"
        );
        let mut output = Vec::new();
        let mut calls = Vec::new();
        serve(input.as_bytes(), &mut output, |action, params| {
            calls.push((action.to_owned(), params));
            Ok(json!({"changed":true}))
        })?;
        assert_eq!(
            calls,
            vec![(
                "audio.switch_playback".into(),
                json!({"device_id":"speaker"})
            )]
        );
        let response: Value = serde_json::from_slice(&output)?;
        assert_eq!(response["id"], "call");
        assert_eq!(response["result"]["isError"], false);
        Ok(())
    }

    #[test]
    fn server_errors_are_tool_results_and_bad_json_recovers() -> Result<()> {
        let input = b"not json\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"app_status\"}}\n";
        let mut output = Vec::new();
        serve(&input[..], &mut output, |_, _| bail!("daemon not running"))?;
        let responses: Vec<Value> = String::from_utf8(output)?
            .lines()
            .map(serde_json::from_str)
            .collect::<std::result::Result<_, _>>()?;
        assert_eq!(responses[0]["error"]["code"], -32700);
        assert_eq!(responses[1]["id"], 2);
        assert_eq!(responses[1]["result"]["isError"], true);
        Ok(())
    }

    #[test]
    fn cli_alias_and_json_boundary() -> Result<()> {
        let args = vec!["diag".into()];
        assert_eq!(parse_cli(&args)?, ("app.status", json!({})));
        assert!(parse_cli(&["app.status".into(), "[]".into()]).is_err());
        assert!(parse_cli(&["app_status".into()]).is_err());
        assert!(parse_cli(&[]).is_err());
        Ok(())
    }
}
