# Orbment（导力匣）

**Orbment（导力匣）** 是一款面向 Windows x64 的原生桌面效率工具箱，使用 **Rust（2024 Edition）** 与 **Win32 + Direct3D 11 + Direct2D + DirectWrite + egui** 构建。

它旨在解决日常高频桌面效率小工具各自常驻后台、各自维护托盘图标与重型运行时（WebView2 / Electron / Qt / .NET）导致的系统托盘拥挤与内存膨胀问题，将剪贴板历史、区域截图与贴图、分轨录屏、音频设备切换与音量旋钮映射、任务栏/悬浮窗网速与硬件监控、NTFS MFT 极速文件搜索与磁盘空间树图、内存清理与内存可视化树图、端口占用快查、文件占用解锁、桌面小工具（窗口置顶、屏幕取色、DDC/CI 外接显示器亮度调节、防休眠）以及 AI 快速提问整合进**单一、统一、按需加载且闲置自动释放显存/内存的原生程序**中。

---

## 快速开始

解压 Windows x64 便携 ZIP 到可写目录，双击 `orbment.exe`，从系统托盘打开设置。预览包未经 Authenticode 签名。当前已在 Windows 11 x64 本机验证，Windows 10 x64 仍待独立验证。

- **启动权限**：普通双击不会请求管理员权限。剪贴板、截图、音频切换等可直接使用。内置 NTFS 索引、内存清理、强制关闭其他进程的句柄、PawnIO 温度等需要额外权限；需要时在「通用与启动」选择「以管理员身份重启」。开机自启需另行开启。
- **剪贴板隐私**：历史保存在本机，识别到的敏感文本默认不收录，已有历史不会自动删除。识别依赖格式匹配；主动开启「记录敏感凭据」时，内容会存原文，界面打码不会加密数据库。
- **数据位置**：便携包自带 `portable.flag`，配置、数据库、剪贴板媒体和缓存保存在程序旁的 `OrbmentData/`。没有标记和现成的 `OrbmentData/` 时，默认使用 `%APPDATA%Orbment`。`--data-dir` 或 `ORBMENT_DATA_DIR` 可以指定实际数据目录，前者优先。AI 的 API Key 单独存于 Windows 凭据管理器。
- **可选组件**：要复用 [Everything](https://www.voidtools.com/) 的索引，先安装并运行 Everything，再在「文件快速搜索」选择「Everything IPC 桥接」。Windows OCR 使用已安装的系统语言支持。可选 ONNX OCR 需要把 `onnxruntime.dll`、`ch_PP-OCRv4_det_infer.onnx`、`ch_PP-OCRv4_rec_infer.onnx` 和 `ppocr_keys_v1.txt` 放进数据目录的 `models/`；便携包不含这些文件。PawnIO 驱动和签名模块也需另行安装。

## 核心特性

### 1. 剪贴板历史管理（`orbment-clipboard`）
- **全格式捕获**：支持纯文本、HTML、RTF 富文本、图片（去重后存为 PNG）与文件列表，识别 Windows 的 `ExcludeClipboardContentFromMonitorProcessing` 和 `CanIncludeInClipboardHistory` 标记，并默认跳过识别到的密码、API Key 等敏感文本。
- **双引擎图片 OCR 索引**：复制图片时可自动通过 **Windows 原生 OCR（`Windows.Media.Ocr`）** 或可选的离线 ONNX 模型提取图中文字并写入 SQLite **FTS5 Trigram** 全文索引，直接搜图内文字即可秒查历史截图。
- **Fluent Dark 竖版弹窗（默认 `Alt+C`）**：
  - 紧凑 420×620 布局，支持分类筛选（全部 / 文本 / 图片 / 文件 / 收藏）与自定义分组；
  - 卡片自适应内容行高，支持十六进制/RGB 色值实时色块预览；
  - **原生 Shell 拖拽导出**：按住剪贴板卡片中的图片或文件直接向外拖拽，即可放入资源管理器文件夹、桌面、聊天窗口或浏览器上传框；
  - 单卡独立复制反馈、双击/回车立即粘贴回原前台窗口、顶部标题栏拖拽记位。

### 2. 区域截图、桌面贴图、OCR、长截图与分轨录屏（`orbment-vision`）
- **智能 UI 元素与视觉边缘识别**：
  - 基于后台缓存的 **UI Automation (`IUIAutomation`) 控件树下钻**、Win32 子窗口枚举以及**纯 Rust 局部视觉边缘/色块连通域检测**，移动鼠标即可精准框选窗口内部的按钮、输入框、列表行与图标，支持滚轮或 `Tab` 在内部子元素与外层容器间逐级切换；
  - 局部脏区合成与帧缓冲复用，在 4K 多显示器高分屏下保持丝滑高帧率且切换元素零明暗闪烁。
- **两阶段截图编辑与双层矢量悬浮工具栏**：
  - 框选松开鼠标后进入编辑态，提供 **8 个圆形缩放控制点**、左上角实时坐标与像素尺寸胶囊，以及按显示器 DPI 自动缩放的**双层圆角矢量图标工具栏**（支持 2× 超采样抗锯齿字体渲染与悬停提示）；
  - **实时画板标注**：支持矩形、圆角矩形、椭圆、画笔、箭头（支持双击箭头添加折线避让批注文本）、文字、递增序号 `①②③`、像素/高斯模糊马赛克、橡皮擦与多步撤销/重做；
  - **一键完成动作**：支持回车/双击直接复制并完成、`F2`（或自定义热键）直接将选区贴到桌面、`Ctrl+S` 原生对话框保存 PNG、发起滚动长截图、区域录屏、OCR 识字或送入 AI 快速提问。
- **多张桌面贴图与贴图历史管理**：
  - 支持在桌面上同时钉住任意多张图片、文本或色卡贴图（坐标重合时自动级联错位），支持滚轮缩放、`Ctrl+滚轮` 调透明度、窗口置顶、鼠标穿透、缩略图折叠及贴图内文字直接框选复制；
  - 双击贴图、按 `Esc`/`Delete` 或点击悬停矢量工具栏的关闭按钮即可销毁贴图；
  - 设置中心内置 **「贴图管理与历史」** 面板，支持查看当前桌面活动贴图与历史已关闭贴图的异步缩略图，随时一键重新贴回桌面、复制、另存为或删除。
- **滚动长截图与区域分轨录屏**：
  - **滚动长截图**：自动锁定底层外部目标窗口注入滚动并实时特征拼接，屏幕上显示排除采集的悬浮提示条，随时按 `Esc` 或鼠标右键即可结束并输出已拼接长图；
  - **区域分轨录屏**：自动抑制系统全屏采集黄框，提供鼠标穿透的区域录制红框与浅色圆角悬浮录屏控制栏（支持开关系统声音/麦克风、切换 `MP4`/`GIF`/`WebP` 格式、实时显示录制计时、暂停/继续并自动补偿时间轴偏移、完成保存与后期裁剪编辑，支持录制键鼠点击涟漪与按键气泡轨迹）。

### 3. 音频设备切换与快捷键（`orbment-audio`）
- **毫秒级设备轮换**：基于 WASAPI `IMMDeviceEnumerator`、`IMMNotificationClient` 事件回调与 `IPolicyConfig` 接口，支持在勾选的常用播放/录音设备间一键循环切换。
- **可选音量旋钮映射**：在「音频切换 → 设备与快捷键」可自定义播放及录音设备的切换快捷键。也可主动开启旋钮映射，将音量旋钮按下发出的系统静音键映射为切换播放设备等动作。
- **场景 Profile 与单应用音频路由**：支持按前台窗口自动切换音频设备，以及基于 `IAudioPolicyConfigFactory` 为指定进程单独绑定输出/输入设备。

### 4. 任务栏与桌面悬浮窗网速及硬件监控（`orbment-monitor`）
- **任务栏嵌入与桌面悬浮窗双模式**：
  - 可在设置中心一键选择 **「任务栏嵌入」**、**「桌面悬浮窗」** 或 **「同时显示」**；
  - 任务栏窗口默认采用经典的 **上下双行多列对齐排布**（亦支持切换单行水平排列），支持 **背景随系统状态栏全透明**（带柔和文字投影适配深浅色任务栏）、根据任务栏颜色自动设置背景色，以及精确调整 Windows 11 任务栏左/右侧预留像素；
  - 桌面悬浮窗兼容经典 INI 皮肤格式，支持贴边自动隐藏、全屏应用自动避让与鼠标穿透。
- **纯 Rust 零 .NET 依赖硬件采样**：
  - 网速采集基于 `GetIfTable2`，CPU/内存/显卡/磁盘利用率基于系统性能计数器与 `D3DKMTQueryAdapterInfo`；
  - 温度采集支持 NVML（NVIDIA）、ADL2（AMD）、SMART / `IOCTL_STORAGE_QUERY_PROPERTY`（磁盘）以及系统原生 PDH Thermal Zone 温度计数器（并支持可选的 PawnIO 签名模块读取 CPU MSR/SMN 温度）；
  - 内置每日上传/下载流量统计落库与历史图表查询。

### 5. NTFS MFT 极速文件搜索、磁盘空间树图与文件解锁（`orbment-analyzer`）
- **双后端文件搜索（默认 `Alt+F` 或双击 `Ctrl`）**：
  - **内置 NTFS `$MFT` + `USN Journal` 引擎**：支持常驻内存、磁盘快照（`mft_snapshot.bin`）+ USN 增量同步、按需扫描三种驻留档位；
  - **Everything 桥接模式（`EverythingBridge`）**：当本机已运行 Everything 时，可直接通过 IPC / CLI 零内存开销复用其索引。
- **磁盘空间嵌套树图（Squarified Treemap）**：
  - 使用 Direct2D 硬件加速绘制多层嵌套块状树图，支持按目录层级下钻、大文件排行榜（Top Files + `blake3` 快速查重）、扩展名占比统计以及安全移入系统回收站。
- **文件与目录占用解锁**：
  - 结合 Windows Restart Manager (`rstrtmgr.dll`) 与 `NtQuerySystemInformation(SystemExtendedHandleInformation)` 句柄表扫描，支持一键查询锁住文件或文件夹的进程，提供结束占用进程或关闭目标句柄解锁。

### 6. 内存清理、内存可视化树图、残留进程回收与端口快查（`orbment-analyzer`）
- **系统内存深度清理**：基于原生 `NtSetSystemInformation` 清理进程工作集、系统文件缓存、备用页面列表（Standby List）与修改页面列表，支持定时或按内存占用百分比阈值静默清理。
- **物理与虚拟内存可视化树图**：将系统总物理内存（进程工作集、系统缓存/备用页、内核分页与非分页池、空闲内存）按固定语义色板渲染为可下钻的嵌套树图；双击任一进程可继续下钻查看其内部 `MEM_IMAGE`（DLL 模块）、`MEM_MAPPED` 与 `MEM_PRIVATE`（堆栈与私有分配）分布。
- **开发工具残留进程清理（Process Reaper）与端口占用快查**：
  - 自动识别脱离父进程的孤儿后台进程（如残留的 `node.exe`、`cargo.exe`、`rust-analyzer.exe`），支持温和关闭与强制清理；
  - 基于 `GetExtendedTcpTable` / `GetExtendedUdpTable` 毫秒级反查占用指定端口（如 `:3000`、`:8080`）的进程路径、命令行与父进程并一键释放端口。

### 7. 桌面小工具、AI 快速提问与统一 MCP / CLI 接口
- **桌面小工具（`utilities`）**：任意窗口置顶切换（带 DPI 自适应高亮描边提示）、屏幕像素放大镜取色器、系统/显示器防休眠常亮开关、外接显示器 DDC/CI + 内屏 WMI 硬件亮度快捷键调节。
- **AI 快速提问小窗（`orbment-quickask`）**：支持 OpenAI Responses、OpenAI 兼容 Chat Completions、Anthropic 与 Google Gemini 流式接口；可保存多个自定义命名渠道，分别管理端点、模型、思考强度与 Windows 凭据管理器中的 API Key，并一键获取模型列表。可随时将框选截图或选中文本送入极简悬浮小窗提问，通过 Tool Calling 直接操控工具箱内部动作。
- **统一外部控制面（Named Pipe IPC + CLI + MCP Server）**：
  - 接口默认关闭。启用后，已注册的工具通过只允许当前 Windows 用户访问的本地管道 `\\.\pipe\orbment-control-v1` 提供服务；高风险操作需要单独授权。
  - 支持 `orbment.exe cli ...` 命令行调用与 `orbment.exe mcp`（基于 `stdio` 的 Model Context Protocol 服务）。

---

## 工程架构与目录结构

```text
Orbment/
├── Cargo.toml                  # Rust Workspace 根配置 (2024 Edition)
├── README.md                   # 项目说明文档
├── assets/
│   └── locales/
│       └── zh-CN.toml          # 界面多语言资源表
├── docs/
│   ├── 01-总体架构与底座设计.md
│   ├── 02-模块功能与设置项对齐规格.md
│   └── 03-分阶段开发路线与施工手册.md
└── crates/
    ├── orbment-core/           # 核心底座：配置 schema、SQLite WAL 数据库、双轨生命周期、ActionBus、IPC、OCR 路由、开机自启与小工具
    ├── orbment-ui/             # 共享 D3D11/Direct2D/DirectWrite 渲染上下文、egui 宿主、Fluent Dark 主题、矢量图标、低级钩子线程与树图画布
    ├── orbment-audio/          # WASAPI 设备枚举/监听、IPolicyConfig 切换、单应用音频路由、旋钮静音键拦截与 OSD
    ├── orbment-monitor/        # 网速与纯 Rust 硬件传感器采样、任务栏双行透明子窗口、桌面悬浮窗与 INI 皮肤解析
    ├── orbment-analyzer/       # NTFS $MFT + USN 扫描引擎、Everything 桥接、Squarified 树图布局、内存清理、残留进程回收、端口快查与文件解锁
    ├── orbment-clipboard/      # 剪贴板监听、隐私/敏感信息过滤、SQLite FTS5 Trigram 存储与读写并发连接池
    ├── orbment-vision/         # 智能 UI 元素识别、区域截图遮罩、双层矢量工具栏、画板标注、多贴图管理、长截图拼接与分轨录屏
    ├── orbment-quickask/       # 四类协议流式客户端、多渠道独立凭据、思考强度与快速提问悬浮小窗
    ├── orbment-mcp/            # Model Context Protocol (stdio JSON-RPC) 服务端与外部权限门禁
    ├── orbment-importer/       # 独立可剥离的一次性旧配置与历史数据迁移工具
    └── orbment-app/            # 单实例守护进程主程序入口、CLI 客户端与模块编排
```

---

## 从源码构建

需要 Windows x64、Rust stable 的 MSVC 工具链（`x86_64-pc-windows-msvc`）、Visual Studio Build Tools 的 C++ 桌面开发组件及 Windows SDK。在仓库根目录运行：

```powershell
# 运行全工作区单元测试
cargo test --workspace --locked

# 代码静态检查
cargo clippy --workspace --all-targets --locked -- -D warnings

# 构建 Release 正式版
cargo build --release --locked
```

编译产物位于：
- 主程序：`target\release\orbment.exe`
- 独立迁移工具（可选）：`target\release\orbment-importer.exe`

### 构建便携包

在 PowerShell 7 中装好当前 Rust 工具链的 `rust-docs` 组件后运行打包脚本。ZIP 会写入 `dist/`，包含 Release 程序、指定静态文件以及按构建依赖生成的第三方声明。Windows CI 会运行同样的脚本并保存 ZIP。旧数据迁移工具单独构建；迁移前先关闭 Orbment，并用 `--dry-run` 预览导入内容。

```powershell
rustup component add rust-docs
./scripts/package-portable.ps1
```

### 接入 MCP 或 CLI

先启动主程序，在「MCP 接口与权限」打开本地接口总开关。CLI 可以用 `orbment.exe cli app.status` 检查连接。使用 MCP 时，在客户端配置中添加以下内容，并把路径替换为解压后的实际位置：

```json
{
  "mcpServers": {
    "orbment": {
      "command": "C:/Tools/Orbment/orbment.exe",
      "args": ["mcp"]
    }
  }
}
```

---

## 许可证与致谢

Orbment 自有代码、文档和图标采用 [MIT](LICENSE)。截图选区的 UI Automation 命中逻辑有一小段改编自 Snow Apps 的 Apache-2.0 版本，保留原许可。见 [第三方声明](THIRD-PARTY-NOTICES.txt)；便携包的 `licenses/` 附有依赖与字体的许可全文。

Orbment 在功能设计、系统接口调用与交互体验上参考并致谢以下优秀的开源项目：

- **[EcoPaste](https://github.com/EcoPasteHub/EcoPaste)**（Apache-2.0）：剪贴板历史分类、分组、隐私脱敏与卡片交互设计参考。
- **[SoundSwitch](https://github.com/Belphemur/SoundSwitch)**（GPL-3.0）：Windows 音频终端枚举、`IPolicyConfig` 默认设备轮换与通知横幅交互参考。
- **[TrafficMonitor](https://github.com/zhongyang219/TrafficMonitor)**（Anti 996 License）：任务栏双行网速/硬件监控排布、桌面悬浮窗 INI 皮肤格式与每日流量统计参考。
- **[Mem Reduct](https://github.com/henrypp/memreduct)**（GPL-3.0）：基于 Windows Native API（`NtSetSystemInformation`）的内存工作集与备用列表清理机制参考。
- **[Snow Shot (`snow-apps`)](https://github.com/mg-chao/snow-apps)**：截图选区和工具栏的设计参考。命中逻辑的改编依据是 [Apache-2.0 的历史提交](https://github.com/mg-chao/snow-apps/tree/e7ba29839163c4dd56aaf12797772a2abf948ece/snow-crates)，现行 Snow Apps 仓库按组件使用不同许可证。
- **[LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor)**（MPL-2.0）与 **[PawnIO](https://pawnio.eu/)**：硬件传感器与处理器温度寄存器读取方案参考。
