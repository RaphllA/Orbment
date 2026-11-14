//! Renders representative UI states through the real D3D11 + Direct2D pipeline and saves the
//! swapchain back buffer as PNG, so visual review does not depend on an unobscured desktop.
use super::*;
use crate::{FileToolsState, analyzer_panel::AnalyzerPanel, file_tools::SearchView};
use orbment_analyzer::{
    disk::{DiskEntry, DiskReport, ExtensionStat},
    file_search::SearchResult,
    memory,
    port::{AddressFamily, PortEntry, Protocol, TcpState},
    process::ProcessIdentity,
    reaper,
    treemap::Node,
};
use orbment_core::config::{MemoryConfig, OrbmentConfig};
use serde_json::json;
use std::path::Path;

/// Frames rendered before the captured one: egui needs a measuring pass, and treemap layouts
/// and thumbnails arrive asynchronously from worker threads.
const SETTLE_FRAMES: usize = 16;
const SETTLE_DELAY: Duration = Duration::from_millis(40);
const SETTINGS_SIZE: [u32; 2] = [1180, 800];
const POPUP_SIZE: [u32; 2] = [420, 620];

impl MainHostWindow {
    fn capture_png(&self, size: [u32; 2], pointer: Option<egui::Pos2>, path: &Path) -> Result<()> {
        let mut image = None;
        for frame in 0..=SETTLE_FRAMES {
            if let Some(position) = pointer {
                self.0
                    .input
                    .borrow_mut()
                    .events
                    .push(Event::PointerMoved(position));
            }
            if frame == SETTLE_FRAMES {
                self.0.render_frame(size, 1.0, Some(&mut image))?;
            } else {
                self.0.render_frame(size, 1.0, None)?;
                std::thread::sleep(SETTLE_DELAY);
            }
        }
        image
            .context("Preview frame produced no image")?
            .save(path)
            .with_context(|| format!("Save {}", path.display()))
    }

    fn with_settings(&self, update: impl FnOnce(&mut SettingsView, &egui::Context)) {
        if let Some(view) = self.0.view.borrow_mut().as_mut() {
            let context = view.context.clone();
            if let ViewBody::Settings(settings) = &mut view.body {
                update(settings, &context);
            }
        }
    }
}

fn lifecycle(search: bool, panel: bool) -> Rc<PopupLifecycle> {
    Rc::new(PopupLifecycle {
        config: RefCell::new(ClipboardConfig::default()),
        events: RefCell::new(Vec::new()),
        hidden: Cell::new(None),
        last_active: Cell::new(HWND::default()),
        position: Cell::new(None),
        fixed: Cell::new(false),
        start: Instant::now(),
        search,
        panel,
    })
}

/// Renders representative settings and tool screenshots plus application-owned popup panels
/// (named by output file stem) into `out_dir`.
pub fn render_ui_previews(
    out_dir: &Path,
    panels: Vec<(String, Box<dyn PopupPanel>)>,
) -> Result<Vec<PathBuf>> {
    std::fs::create_dir_all(out_dir).with_context(|| format!("Create {}", out_dir.display()))?;
    let graphics = Rc::new(D2dContext::new()?);
    let mut written = Vec::new();
    let mut save = |host: &MainHostWindow, name: &str, size, pointer| -> Result<()> {
        let path = out_dir.join(format!("{name}.png"));
        host.capture_png(size, pointer, &path)?;
        written.push(path);
        Ok(())
    };

    let config = sample_config()?;
    for (name, page, prepare) in [
        ("01-settings-general", "app", SettingsSample::Plain),
        ("02-settings-audio", "audio", SettingsSample::Audio),
        ("03-settings-monitor", "monitor", SettingsSample::Plain),
        (
            "04-memory-treemap",
            "memory_analyzer",
            SettingsSample::Memory,
        ),
        ("05-memory-ports", "memory_ports", SettingsSample::Ports),
        ("06-disk-treemap", "disk_analysis", SettingsSample::Disk),
        ("10-quickask-channels", "quick_ask", SettingsSample::Plain),
        (
            "11-quickask-prompts",
            "quick_ask",
            SettingsSample::QuickAskPrompts,
        ),
    ] {
        let mut view = SettingsView::new(config.clone());
        view.selected = page.into();
        let host = MainHostWindow::build(
            graphics.clone(),
            ViewBody::Settings(Box::new(view)),
            HWND::default(),
            None,
        )?;
        host.with_settings(|settings, context| prepare.apply(settings, context, &config));
        let size = if matches!(prepare, SettingsSample::QuickAskPrompts) {
            [1180, 1320]
        } else {
            SETTINGS_SIZE
        };
        save(&host, name, size, None)?;
    }

    let clipboard = MainHostWindow::build(
        graphics.clone(),
        ViewBody::Clipboard(Box::new(ClipboardView::with_rows(
            ClipboardConfig::default(),
            sample_clipboard_rows()?,
            vec![
                ClipboardGroup {
                    id: "work".into(),
                    name: "工作".into(),
                    is_hidden: false,
                },
                ClipboardGroup {
                    id: "snippets".into(),
                    name: "代码片段".into(),
                    is_hidden: false,
                },
            ],
            0,
        ))),
        HWND::default(),
        Some(lifecycle(false, false)),
    )?;
    save(
        &clipboard,
        "07-clipboard-popup",
        POPUP_SIZE,
        Some(egui::pos2(220.0, 300.0)),
    )?;

    let state: crate::SharedFileToolsState = Default::default();
    {
        let mut state = state
            .lock()
            .map_err(|_| anyhow::anyhow!("Preview file tools state poisoned"))?;
        state.search_results = sample_search_results();
        state.search_query = Some("orbment".into());
    }
    let search = MainHostWindow::build(
        graphics.clone(),
        ViewBody::Search(Box::new(SearchView::with_query(state, "orbment", 0))),
        HWND::default(),
        Some(lifecycle(true, false)),
    )?;
    save(&search, "08-search-popup", [900, 620], None)?;

    for (name, panel) in panels {
        let host = MainHostWindow::build(
            graphics.clone(),
            ViewBody::Panel(Rc::new(RefCell::new(panel))),
            HWND::default(),
            Some(lifecycle(false, true)),
        )?;
        save(&host, &name, [900, 620], None)?;
    }
    Ok(written)
}

#[derive(Clone, Copy)]
enum SettingsSample {
    Plain,
    Audio,
    Memory,
    Ports,
    Disk,
    QuickAskPrompts,
}

impl SettingsSample {
    fn apply(self, settings: &mut SettingsView, context: &egui::Context, config: &Value) {
        settings.elevated = true;
        match self {
            Self::Plain => {}
            Self::QuickAskPrompts => settings.select_form_tab("quick_ask", 1),
            Self::Audio => {
                settings.audio_devices = json!({
                    "playback": [
                        {"id": "{0.0.0.00000000}.{speakers}", "name": "扬声器 (Realtek High Definition Audio)"},
                        {"id": "{0.0.0.00000000}.{headset}", "name": "耳机 (Sony WH-1000XM5)"},
                        {"id": "{0.0.0.00000000}.{monitor}", "name": "DELL U2723QE (NVIDIA High Definition Audio)"},
                    ],
                    "recording": [
                        {"id": "{0.0.1.00000000}.{mic}", "name": "麦克风阵列 (Realtek)"},
                        {"id": "{0.0.1.00000000}.{usb}", "name": "USB 麦克风 (Blue Yeti)"},
                    ],
                    "default_playback": "{0.0.0.00000000}.{headset}",
                    "default_recording": "{0.0.1.00000000}.{usb}",
                });
            }
            Self::Memory | Self::Ports => {
                let snapshot = memory::snapshot().ok();
                let residuals = reaper::preview(&MemoryConfig::default().reaper).ok();
                let ports = if matches!(self, Self::Ports) {
                    sample_ports()
                } else {
                    Vec::new()
                };
                settings.analyzer = Some(AnalyzerPanel::with_data(
                    &settings.selected,
                    config.clone(),
                    context.clone(),
                    snapshot,
                    residuals,
                    ports,
                ));
                settings.active_page = settings.selected.clone();
            }
            Self::Disk => {
                let state = FileToolsState {
                    disk: Some(sample_disk_report().into()),
                    disk_revision: 1,
                    revision: 1,
                    ..Default::default()
                };
                settings.file_tools_state = Arc::new(std::sync::Mutex::new(state));
            }
        }
    }
}

fn sample_config() -> Result<Value> {
    let mut config = serde_json::to_value(OrbmentConfig::default())?;
    config["audio"]["selected_playback_devices"] = json!([
        {"id": "{0.0.0.00000000}.{speakers}", "name": "扬声器 (Realtek High Definition Audio)"},
        {"id": "{0.0.0.00000000}.{headset}", "name": "耳机 (Sony WH-1000XM5)"},
        {"id": "{0.0.0.00000000}.{hdmi}", "name": "客厅电视 (HDMI)"},
    ]);
    config["audio"]["selected_recording_devices"] =
        json!([{"id": "{0.0.1.00000000}.{usb}", "name": "USB 麦克风 (Blue Yeti)"}]);
    config["memory"]["enabled"] = json!(true);
    config["disk_analyzer"]["enabled"] = json!(true);
    config["quick_ask"]["enabled"] = json!(true);
    config["quick_ask"]["system_prompt"] = json!(
        "你是一位严谨、友好的桌面助手。请使用简体中文回答，先给出结论，再提供必要的步骤与解释。"
    );
    // COLORREF (0x00BBGGRR) values so the swatch grid shows a realistic palette.
    let palette = [
        0x00F6823B_u32,
        0x0099D334,
        0x000B9EF5,
        0x004444EF,
        0x00FA8BA7,
        0x00D4B606,
        0x003C92FB,
        0x0094A3B8,
        0x0081C784,
        0x00F7C14F,
        0x00DB7BF0,
        0x00F4F3F1,
        0x0080DE4A,
    ];
    // The floating grid is expanded and the taskbar grid collapsed on the monitor page.
    config["monitor"]["taskbar"]["enabled"] = json!(true);
    config["monitor"]["floating"]["specify_each_item_color"] = json!(true);
    config["monitor"]["taskbar"]["specify_each_item_color"] = json!(false);
    config["monitor"]["floating"]["text_colors"] = json!(palette);
    config["monitor"]["taskbar"]["text_colors"] =
        json!(palette.iter().chain(palette.iter()).collect::<Vec<_>>());
    config["monitor"]["taskbar"]["back_color"] = json!(0x001C1918_u32);
    Ok(config)
}

fn sample_clipboard_rows() -> Result<Vec<ClipboardRow>> {
    let image_path = std::env::temp_dir().join("orbment-preview-clipboard.png");
    let image = image::RgbaImage::from_fn(480, 270, |x, y| {
        let t = x as f32 / 480.0;
        let v = y as f32 / 270.0;
        image::Rgba([
            (40.0 + 80.0 * t) as u8,
            (90.0 + 90.0 * v) as u8,
            (200.0 - 60.0 * t) as u8,
            255,
        ])
    });
    image.save(&image_path)?;
    let row = |id: &str, kind: &str, content: &str, source: &str, created_at: &str| ClipboardRow {
        id: id.into(),
        kind: kind.into(),
        content: content.into(),
        summary: None,
        note: None,
        is_sensitive: false,
        is_favorite: false,
        is_pinned: false,
        image_path: None,
        source: Some(source.into()),
        size: Some(content.len() as u64),
        created_at: Some(created_at.into()),
    };
    Ok(vec![
        ClipboardRow {
            is_favorite: true,
            ..row(
                "1",
                "text",
                "fn main() {\n    let theme = Theme::fluent_dark();\n    println!(\"Hello, Orbment! {theme:?}\");\n}",
                "Visual Studio Code",
                "2026-10-21T01:42:10Z",
            )
        },
        row("2", "text", "#3B82F6", "Figma", "2026-10-21T01:40:02Z"),
        ClipboardRow {
            image_path: Some(image_path),
            size: Some(186_420),
            ..row("3", "image", "", "截图工具", "2026-10-21T01:31:45Z")
        },
        ClipboardRow {
            is_pinned: true,
            ..row(
                "4",
                "files",
                r#"["F:\\Orbment\\docs\\ui_design_spec.md","F:\\Orbment\\Cargo.toml","F:\\Orbment\\crates\\orbment-ui\\src\\theme.rs"]"#,
                "资源管理器",
                "2026-10-20T13:05:00Z",
            )
        },
        row(
            "5",
            "html",
            "周会纪要：1. 设置中心改为分组导航与卡片化表单；2. 剪贴板弹窗采用圆角卡片；3. 下周验收 GPU 截图。",
            "Microsoft Edge",
            "2026-10-20T09:12:30Z",
        ),
        row(
            "6",
            "text",
            "https://github.com/emilk/egui/releases/tag/0.31.1",
            "Google Chrome",
            "2026-10-19T15:20:00Z",
        ),
    ])
}

fn sample_search_results() -> Vec<SearchResult> {
    let result = |path: &str, size: u64, directory: bool| SearchResult {
        path: path.into(),
        name: Path::new(path)
            .file_name()
            .map_or_else(|| path.into(), |name| name.to_string_lossy().into_owned()),
        logical_size: size,
        allocated_size: size,
        is_directory: directory,
        score: 0,
    };
    vec![
        result(r"F:\Orbment", 0, true),
        result(r"F:\Orbment\target\release\orbment.exe", 18_874_368, false),
        result(r"F:\Orbment\Cargo.toml", 2_048, false),
        result(r"F:\Orbment\crates\orbment-ui\src\theme.rs", 17_502, false),
        result(r"F:\Orbment\docs\orbment-architecture.md", 48_230, false),
        result(r"D:\Backup\orbment-2026-10-20.zip", 734_003_200, false),
        result(r"C:\Users\Dev\Pictures\orbment-logo.png", 412_331, false),
        result(r"C:\Users\Dev\AppData\Roaming\Orbment", 0, true),
        result(
            r"C:\Users\Dev\Videos\orbment-demo.mp4",
            1_288_490_188,
            false,
        ),
    ]
}

fn sample_ports() -> Vec<PortEntry> {
    let entry = |protocol: Protocol,
                 port: u16,
                 state: Option<TcpState>,
                 pid: u32,
                 name: &str,
                 path: &str,
                 remote: Option<(&str, u16)>| PortEntry {
        protocol,
        address_family: AddressFamily::Ipv4,
        local_address: if remote.is_some() {
            "192.168.1.20"
        } else {
            "0.0.0.0"
        }
        .into(),
        local_port: port,
        remote_address: remote.map(|(address, _)| address.into()),
        remote_port: remote.map(|(_, port)| port),
        state,
        pid,
        identity: Some(ProcessIdentity {
            pid,
            creation_time: 133_000_000_000_000_000 + u64::from(pid),
        }),
        process_name: Some(name.into()),
        path: Some(path.into()),
        command_line: Some(format!("\"{path}\" --port {port}")),
    };
    vec![
        entry(
            Protocol::Tcp,
            3000,
            Some(TcpState::Listen),
            18244,
            "node.exe",
            r"C:\Program Files\nodejs\node.exe",
            None,
        ),
        entry(
            Protocol::Tcp,
            5173,
            Some(TcpState::Listen),
            20912,
            "node.exe",
            r"C:\Program Files\nodejs\node.exe",
            None,
        ),
        entry(
            Protocol::Tcp,
            8080,
            Some(TcpState::Listen),
            9620,
            "java.exe",
            r"C:\Program Files\Java\jdk-21\bin\java.exe",
            None,
        ),
        entry(
            Protocol::Tcp,
            5432,
            Some(TcpState::Listen),
            4410,
            "postgres.exe",
            r"C:\Program Files\PostgreSQL\16\bin\postgres.exe",
            None,
        ),
        entry(
            Protocol::Tcp,
            51234,
            Some(TcpState::Established),
            13372,
            "Code.exe",
            r"C:\Users\Dev\AppData\Local\Programs\Microsoft VS Code\Code.exe",
            Some(("140.82.112.21", 443)),
        ),
        entry(
            Protocol::Udp,
            5353,
            None,
            2876,
            "svchost.exe",
            r"C:\Windows\System32\svchost.exe",
            None,
        ),
        entry(
            Protocol::Tcp,
            6379,
            Some(TcpState::TimeWait),
            7788,
            "redis-server.exe",
            r"C:\Tools\Redis\redis-server.exe",
            None,
        ),
    ]
}

fn sample_disk_report() -> DiskReport {
    fn node(path: &str, bytes: u64, children: Vec<Node>) -> Node {
        Node {
            id: path.into(),
            label: Path::new(path)
                .file_name()
                .map_or_else(|| path.into(), |name| name.to_string_lossy().into_owned()),
            bytes: if children.is_empty() {
                bytes
            } else {
                children.iter().map(|child| child.bytes).sum()
            },
            children,
        }
    }
    let gib = 1u64 << 30;
    let mib = 1u64 << 20;
    let root = node(
        r"C:\Users\Dev",
        0,
        vec![
            node(
                r"C:\Users\Dev\Videos",
                0,
                vec![
                    node(r"C:\Users\Dev\Videos\orbment-demo.mp4", 14 * gib, vec![]),
                    node(r"C:\Users\Dev\Videos\会议录屏-1020.mkv", 9 * gib, vec![]),
                    node(r"C:\Users\Dev\Videos\travel-2026.mov", 6 * gib, vec![]),
                    node(r"C:\Users\Dev\Videos\clip.webm", 800 * mib, vec![]),
                ],
            ),
            node(
                r"C:\Users\Dev\Projects",
                0,
                vec![
                    node(
                        r"C:\Users\Dev\Projects\orbment",
                        0,
                        vec![
                            node(r"C:\Users\Dev\Projects\orbment\target", 11 * gib, vec![]),
                            node(
                                r"C:\Users\Dev\Projects\orbment\assets.zip",
                                1200 * mib,
                                vec![],
                            ),
                            node(r"C:\Users\Dev\Projects\orbment\crates", 300 * mib, vec![]),
                        ],
                    ),
                    node(
                        r"C:\Users\Dev\Projects\web-app",
                        0,
                        vec![
                            node(
                                r"C:\Users\Dev\Projects\web-app\node_modules",
                                4 * gib,
                                vec![],
                            ),
                            node(r"C:\Users\Dev\Projects\web-app\dist", 600 * mib, vec![]),
                        ],
                    ),
                ],
            ),
            node(
                r"C:\Users\Dev\Downloads",
                0,
                vec![
                    node(r"C:\Users\Dev\Downloads\Win11_24H2.iso", 6 * gib, vec![]),
                    node(
                        r"C:\Users\Dev\Downloads\VisualStudioSetup.exe",
                        3 * gib,
                        vec![],
                    ),
                    node(r"C:\Users\Dev\Downloads\dataset.7z", 2 * gib, vec![]),
                    node(r"C:\Users\Dev\Downloads\manual.pdf", 400 * mib, vec![]),
                ],
            ),
            node(
                r"C:\Users\Dev\Pictures",
                0,
                vec![
                    node(r"C:\Users\Dev\Pictures\RAW", 5 * gib, vec![]),
                    node(r"C:\Users\Dev\Pictures\wallpaper.png", 900 * mib, vec![]),
                    node(r"C:\Users\Dev\Pictures\screenshots.jpg", 700 * mib, vec![]),
                ],
            ),
            node(
                r"C:\Users\Dev\AppData",
                0,
                vec![
                    node(r"C:\Users\Dev\AppData\Local", 7 * gib, vec![]),
                    node(r"C:\Users\Dev\AppData\Roaming", 2 * gib, vec![]),
                ],
            ),
            node(r"C:\Users\Dev\Documents", 3 * gib, vec![]),
        ],
    );
    let entry = |path: &str, size: u64| DiskEntry {
        path: path.into(),
        name: Path::new(path)
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
        logical_size: size,
        allocated_size: size,
        is_directory: false,
        file_ref_number: 0,
        volume_serial: 0,
    };
    let extension = |extension: &str, count: u64, size: u64| ExtensionStat {
        extension: extension.into(),
        file_count: count,
        logical_size: size,
        allocated_size: size,
    };
    DiskReport {
        target: root.id.clone(),
        logical_size: root.bytes,
        allocated_size: root.bytes + 3 * gib,
        file_count: 284_913,
        directory_count: 41_207,
        top_files: vec![
            entry(r"C:\Users\Dev\Videos\orbment-demo.mp4", 14 * gib),
            entry(r"C:\Users\Dev\Videos\会议录屏-1020.mkv", 9 * gib),
            entry(r"C:\Users\Dev\Downloads\Win11_24H2.iso", 6 * gib),
        ],
        extensions: vec![
            extension("mp4", 42, 18 * gib),
            extension("iso", 2, 6 * gib),
            extension("zip", 31, 3 * gib),
        ],
        treemap: root,
    }
}
