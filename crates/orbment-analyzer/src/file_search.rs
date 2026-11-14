use crate::mft_engine::MftTree;
use orbment_core::config::FileSearchConfig;
use serde::Serialize;
use std::collections::HashMap;

#[derive(Clone, Debug, Serialize)]
pub struct SearchResult {
    pub path: String,
    pub name: String,
    pub logical_size: u64,
    /// Everything's IPC does not expose allocation size; zero means unavailable there.
    pub allocated_size: u64,
    pub is_directory: bool,
    pub score: u64,
}

pub(crate) fn normalized_path(path: &str) -> String {
    let path = path.replace('/', "\\").to_lowercase();
    let path = path
        .strip_prefix("\\\\?\\unc\\")
        .map(|p| format!("\\\\{p}"))
        .unwrap_or_else(|| path.strip_prefix("\\\\?\\").unwrap_or(&path).to_owned());
    path.trim_end_matches('\\').to_owned()
}

pub(crate) fn within_path(path: &str, parent: &str) -> bool {
    path == parent
        || path
            .strip_prefix(parent)
            .is_some_and(|rest| rest.starts_with('\\'))
}

#[derive(Clone, Debug)]
enum Predicate {
    Text(String),
    Extension(Vec<String>),
    Size(Bounds),
    Modified(Bounds),
    Directory(bool),
}

#[derive(Clone, Debug)]
struct Term {
    negate: bool,
    predicate: Predicate,
}

#[derive(Clone, Copy, Debug)]
struct Bounds {
    min: i128,
    max: i128,
}

impl Bounds {
    fn contains(self, value: i128) -> bool {
        value >= self.min && value <= self.max
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Query(Vec<Term>);

impl Query {
    pub(crate) fn parse(input: &str) -> Result<Self, String> {
        let mut terms = Vec::new();
        for token in tokenize(input)? {
            let (negate, token) = token
                .strip_prefix('!')
                .map_or((false, token.as_str()), |s| (true, s));
            if token.is_empty() {
                return Err("Missing expression after !".into());
            }
            let lower = token.to_lowercase();
            let predicate = if let Some(value) = lower.strip_prefix("ext:") {
                let extensions: Vec<_> = value
                    .split(';')
                    .map(|s| s.trim_start_matches('.').to_owned())
                    .collect();
                if extensions.iter().any(String::is_empty) {
                    return Err("ext: requires a semicolon-separated extension list".into());
                }
                Predicate::Extension(extensions)
            } else if let Some(value) = lower.strip_prefix("size:") {
                Predicate::Size(parse_bounds(value, parse_size)?)
            } else if lower.starts_with(['>', '<', '=']) {
                Predicate::Size(parse_bounds(&lower, parse_size)?)
            } else if let Some(value) = lower
                .strip_prefix("dm:")
                .or_else(|| lower.strip_prefix("modified:"))
                .or_else(|| lower.strip_prefix("mtime:"))
                .or_else(|| lower.strip_prefix("date:"))
            {
                Predicate::Modified(parse_bounds(value, parse_time)?)
            } else if lower == "file:" {
                Predicate::Directory(false)
            } else if lower == "folder:" {
                Predicate::Directory(true)
            } else {
                Predicate::Text(lower.replace('/', "\\"))
            };
            terms.push(Term { negate, predicate });
        }
        Ok(Self(terms))
    }

    pub(crate) fn score(
        &self,
        name: &str,
        path: &str,
        size: u64,
        modified: i64,
        directory: bool,
    ) -> Option<u64> {
        let name = name.to_lowercase();
        let path = normalized_path(path);
        let mut score = 0u64;
        for term in &self.0 {
            let (matches, weight) = match &term.predicate {
                Predicate::Text(text) => {
                    let haystack = if text.contains(['\\', ':']) {
                        path.as_str()
                    } else {
                        name.as_str()
                    };
                    if text.contains(['*', '?']) {
                        (wildcard(text, haystack), 200)
                    } else {
                        let weight = if haystack == text {
                            1000
                        } else if haystack.starts_with(text) {
                            500
                        } else {
                            100
                        };
                        (haystack.contains(text), weight)
                    }
                }
                Predicate::Extension(extensions) => {
                    let ext = name.rsplit_once('.').map(|(_, ext)| ext);
                    (
                        !directory
                            && ext.is_some_and(|ext| {
                                extensions.iter().any(|candidate| candidate == ext)
                            }),
                        0,
                    )
                }
                Predicate::Size(bounds) => (!directory && bounds.contains(i128::from(size)), 0),
                Predicate::Modified(bounds) => (bounds.contains(i128::from(modified)), 0),
                Predicate::Directory(expected) => (directory == *expected, 0),
            };
            if matches == term.negate {
                return None;
            }
            if !term.negate {
                score = score.saturating_add(weight);
            }
        }
        Some(score)
    }
}

fn tokenize(input: &str) -> Result<Vec<String>, String> {
    if input.contains('\0') {
        return Err("Search query contains a NUL character".into());
    }
    let mut quoted = false;
    let mut token = String::new();
    let mut tokens = Vec::new();
    for ch in input.chars() {
        if ch == '"' {
            quoted = !quoted;
        } else if ch.is_whitespace() && !quoted {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        } else {
            token.push(ch);
        }
    }
    if quoted {
        return Err("Unterminated quoted search phrase".into());
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    Ok(tokens)
}

fn parse_bounds(value: &str, parse: fn(&str) -> Result<Bounds, String>) -> Result<Bounds, String> {
    if let Some((low, high)) = value.split_once("..") {
        let min = if low.is_empty() {
            i128::MIN
        } else {
            parse(low)?.min
        };
        let max = if high.is_empty() {
            i128::MAX
        } else {
            parse(high)?.max
        };
        if min > max || (low.is_empty() && high.is_empty()) {
            return Err(format!("Invalid filter range: {value}"));
        }
        return Ok(Bounds { min, max });
    }
    let (operator, operand) = [">=", "<=", ">", "<", "="]
        .into_iter()
        .find_map(|op| value.strip_prefix(op).map(|v| (op, v)))
        .unwrap_or(("=", value));
    let parsed = parse(operand)?;
    Ok(match operator {
        ">" => Bounds {
            min: parsed.max + 1,
            max: i128::MAX,
        },
        ">=" => Bounds {
            min: parsed.min,
            max: i128::MAX,
        },
        "<" => Bounds {
            min: i128::MIN,
            max: parsed.min - 1,
        },
        "<=" => Bounds {
            min: i128::MIN,
            max: parsed.max,
        },
        _ => parsed,
    })
}

fn parse_size(input: &str) -> Result<Bounds, String> {
    let split = input
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(input.len());
    let (number, unit) = input.split_at(split);
    let multiplier = match unit {
        "" | "b" => 1u64,
        "k" | "kb" | "kib" => 1 << 10,
        "m" | "mb" | "mib" => 1 << 20,
        "g" | "gb" | "gib" => 1 << 30,
        "t" | "tb" | "tib" => 1 << 40,
        "p" | "pb" | "pib" => 1 << 50,
        _ => return Err(format!("Invalid size unit: {input}")),
    };
    let (integer, fraction) = number.split_once('.').unwrap_or((number, ""));
    let invalid = || format!("Invalid size: {input}");
    if integer.is_empty() || fraction.len() > 9 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    let integer = integer.parse::<u64>().map_err(|_| invalid())?;
    let fraction_value = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u64>().map_err(|_| invalid())?
    };
    let scale = 10u64.pow(fraction.len() as u32);
    let fractional_bytes =
        (u128::from(fraction_value) * u128::from(multiplier)) / u128::from(scale);
    let value = u128::from(integer) * u128::from(multiplier) + fractional_bytes;
    if value > u128::from(u64::MAX) {
        return Err(invalid());
    }
    Ok(Bounds {
        min: value as i128,
        max: value as i128,
    })
}

// Calendar dates are UTC; a bare date matches the entire day, not just midnight.
fn parse_time(input: &str) -> Result<Bounds, String> {
    let input = input.strip_suffix('z').unwrap_or(input);
    let (date, time) = input
        .split_once('t')
        .map_or((input, None), |(d, t)| (d, Some(t)));
    let invalid =
        || format!("Invalid UTC date (expected YYYY-MM-DD or YYYY-MM-DDTHH:MM:SS): {input}");
    let fields: Vec<_> = date.split('-').collect();
    if fields.len() != 3 || fields[0].len() != 4 || fields[1].len() != 2 || fields[2].len() != 2 {
        return Err(invalid());
    }
    let year = fields[0].parse::<i64>().map_err(|_| invalid())?;
    let month = fields[1].parse::<i64>().map_err(|_| invalid())?;
    let day = fields[2].parse::<i64>().map_err(|_| invalid())?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1601..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || day < 1
        || day > days[(month - 1) as usize]
    {
        return Err(invalid());
    }
    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year / 400;
    let yoe = adjusted_year - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let unix_days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468;
    let seconds = if let Some(time) = time {
        let parts: Vec<_> = time.split(':').collect();
        if parts.len() != 3 || parts.iter().any(|p| p.len() != 2) {
            return Err(invalid());
        }
        let hour = parts[0].parse::<u32>().map_err(|_| invalid())?;
        let minute = parts[1].parse::<u32>().map_err(|_| invalid())?;
        let second = parts[2].parse::<u32>().map_err(|_| invalid())?;
        if hour > 23 || minute > 59 || second > 59 {
            return Err(invalid());
        }
        i64::from(hour * 3600 + minute * 60 + second)
    } else {
        0
    };
    let min = i128::from(unix_days * 86400 + seconds + 11_644_473_600) * 10_000_000;
    Ok(Bounds {
        min,
        max: min
            + if time.is_some() {
                10_000_000 - 1
            } else {
                864_000_000_000 - 1
            },
    })
}

fn wildcard(pattern: &str, value: &str) -> bool {
    let pattern: Vec<_> = pattern.chars().collect();
    let value: Vec<_> = value.chars().collect();
    let (mut p, mut v, mut star, mut retry) = (0, 0, None, 0);
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            p += 1;
            retry = v;
        } else if let Some(index) = star {
            retry += 1;
            v = retry;
            p = index + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

pub fn query_builtin(
    tree: &MftTree,
    config: &FileSearchConfig,
    query: &str,
    limit: usize,
    history: &HashMap<String, u64>,
) -> Result<Vec<SearchResult>, String> {
    let query = Query::parse(query)?;
    if limit == 0 {
        return Ok(Vec::new());
    }
    let included: Vec<_> = config
        .included_drives
        .iter()
        .map(|p| {
            let p = normalized_path(p);
            if p.len() == 1 { format!("{p}:") } else { p }
        })
        .collect();
    let excluded: Vec<_> = config
        .exclude_paths
        .iter()
        .map(|p| normalized_path(p))
        .collect();
    let mut normalized_history = HashMap::<String, u64>::new();
    for (path, count) in history {
        normalized_history
            .entry(normalized_path(path))
            .and_modify(|old| *old = (*old).max(*count))
            .or_insert(*count);
    }
    let mut results = Vec::new();
    for (index, node) in tree.nodes.iter().enumerate() {
        let path = tree.path(index).to_string_lossy().into_owned();
        let normalized = normalized_path(&path);
        if (!included.is_empty() && !included.iter().any(|root| within_path(&normalized, root)))
            || excluded.iter().any(|root| within_path(&normalized, root))
        {
            continue;
        }
        let name = tree.name(index);
        let is_directory = node.flags & 1 != 0;
        if let Some(score) = query.score(
            name,
            &path,
            node.logical_size,
            node.modified_time_ns100,
            is_directory,
        ) {
            results.push(SearchResult {
                path,
                name: name.to_owned(),
                logical_size: node.logical_size,
                allocated_size: node.allocated_size,
                is_directory,
                score: score.saturating_add(
                    normalized_history
                        .get(&normalized)
                        .copied()
                        .unwrap_or(0)
                        .saturating_mul(10_000),
                ),
            });
        }
    }
    let order = |a: &SearchResult, b: &SearchResult| {
        b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path))
    };
    if results.len() > limit {
        results.select_nth_unstable_by(limit, order);
        results.truncate(limit);
    }
    results.sort_unstable_by(order);
    Ok(results)
}

const IPC_REPLY: u32 = 0x4f52424d;
const IPC_REQUEST_FLAGS: u32 = 0x4 | 0x10;

// Public Everything SDK QUERY2W: seven DWORDs, then NUL-terminated UTF-16.
fn ipc_query(reply_hwnd: u32, query: &str, limit: usize) -> Result<Vec<u8>, String> {
    if query.contains('\0') {
        return Err("Search query contains a NUL character".into());
    }
    let max_results =
        u32::try_from(limit).map_err(|_| "Everything result limit exceeds u32".to_owned())?;
    let mut bytes = Vec::new();
    for field in [
        reply_hwnd,
        IPC_REPLY,
        0,
        0,
        max_results,
        IPC_REQUEST_FLAGS,
        1,
    ] {
        bytes.extend_from_slice(&field.to_le_bytes());
    }
    for character in query.encode_utf16().chain(Some(0)) {
        bytes.extend_from_slice(&character.to_le_bytes());
    }
    u32::try_from(bytes.len()).map_err(|_| "Everything query is too large".to_owned())?;
    Ok(bytes)
}

fn ipc_results(bytes: &[u8], limit: usize) -> Result<Vec<SearchResult>, String> {
    fn word(bytes: &[u8], offset: usize) -> Result<u32, String> {
        let data = bytes
            .get(offset..offset + 4)
            .ok_or("Truncated Everything reply")?;
        Ok(u32::from_le_bytes(
            data.try_into().map_err(|_| "Invalid Everything DWORD")?,
        ))
    }
    if bytes.len() < 20 {
        return Err("Truncated Everything list header".into());
    }
    let total = word(bytes, 0)?;
    let offset = word(bytes, 8)?;
    let count = word(bytes, 4)? as usize;
    if u64::from(offset) + count as u64 > u64::from(total) {
        return Err("Invalid Everything result range".into());
    }
    let flags = word(bytes, 12)?;
    if flags != IPC_REQUEST_FLAGS {
        return Err(format!("Unexpected Everything reply fields: {flags:#x}"));
    }
    let data_start = count
        .checked_mul(8)
        .and_then(|n| n.checked_add(20))
        .ok_or("Everything result count overflow")?;
    if data_start > bytes.len() || count > limit {
        return Err("Invalid Everything result count".into());
    }
    let mut results = Vec::with_capacity(count);
    for index in 0..count {
        let item_flags = word(bytes, 20 + index * 8)?;
        let offset = word(bytes, 24 + index * 8)? as usize;
        if offset < data_start {
            return Err("Invalid Everything data offset".into());
        }
        let length = word(bytes, offset)? as usize;
        let end = length
            .checked_mul(2)
            .and_then(|n| n.checked_add(offset + 4))
            .ok_or("Everything string length overflow")?;
        let name_bytes = bytes
            .get(offset + 4..end)
            .ok_or("Truncated Everything filename")?;
        if bytes.get(end..end + 2) != Some(&[0, 0][..]) {
            return Err("Everything filename is not NUL-terminated".into());
        }
        let utf16: Vec<_> = name_bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .collect();
        let path = String::from_utf16_lossy(&utf16);
        let size_bytes = bytes
            .get(end + 2..end + 10)
            .ok_or("Truncated Everything file size")?;
        let size = i64::from_le_bytes(
            size_bytes
                .try_into()
                .map_err(|_| "Invalid Everything size")?,
        );
        let name = path
            .trim_end_matches(['\\', '/'])
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or(&path)
            .to_owned();
        results.push(SearchResult {
            path,
            name,
            logical_size: size.max(0) as u64,
            allocated_size: 0,
            is_directory: item_flags & 7 != 0,
            score: 0,
        });
    }
    Ok(results)
}

pub fn query_everything(
    config: &FileSearchConfig,
    query: &str,
    limit: usize,
) -> Result<Vec<SearchResult>, String> {
    // A dedicated short-lived thread owns the hidden window and pumps only its messages.
    // This also keeps synchronous WM_COPYDATA reentrancy out of the application's UI thread.
    let config = config.clone();
    let query = query.to_owned();
    std::thread::Builder::new()
        .name("orbment-everything-query".into())
        .spawn(move || everything::query(&config, &query, limit))
        .map_err(|e| format!("Start Everything IPC thread: {e}"))?
        .join()
        .map_err(|_| "Everything IPC thread panicked".to_owned())?
}

mod everything {
    use super::*;
    use std::{
        cell::RefCell,
        ffi::c_void,
        path::PathBuf,
        process::Command,
        sync::LazyLock,
        time::{Duration, Instant},
    };
    use windows::{
        Win32::{
            Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
            System::{DataExchange::COPYDATASTRUCT, LibraryLoader::GetModuleHandleW},
            UI::WindowsAndMessaging::*,
        },
        core::{PCWSTR, w},
    };

    struct Reply {
        limit: usize,
        result: RefCell<Option<Result<Vec<SearchResult>, String>>>,
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe {
            if message == WM_NCCREATE {
                let create = &*(lparam.0 as *const CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
            }
            let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const Reply;
            if message == WM_COPYDATA && !state.is_null() && lparam.0 != 0 {
                let state = &*state;
                let data = &*(lparam.0 as *const COPYDATASTRUCT);
                if data.dwData != IPC_REPLY as usize {
                    return LRESULT(0);
                }
                if data.lpData.is_null() || data.cbData > 64 * 1024 * 1024 {
                    *state.result.borrow_mut() =
                        Some(Err("Invalid Everything reply buffer".into()));
                    return LRESULT(0);
                }
                let bytes =
                    std::slice::from_raw_parts(data.lpData.cast::<u8>(), data.cbData as usize);
                *state.result.borrow_mut() = Some(ipc_results(bytes, state.limit));
                return LRESULT(1);
            }
            if message == WM_NCDESTROY {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
    }

    struct ReplyWindow(HWND);
    impl Drop for ReplyWindow {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.0);
            }
        }
    }

    fn discover_window() -> Option<HWND> {
        unsafe {
            FindWindowW(w!("EVERYTHING_TASKBAR_NOTIFICATION"), PCWSTR::null())
                .ok()
                .or_else(|| {
                    FindWindowW(w!("EVERYTHING_TASKBAR_NOTIFICATION_(1.5a)"), PCWSTR::null()).ok()
                })
        }
    }

    fn discover_executable(config: &FileSearchConfig) -> Result<PathBuf, String> {
        if !config.everything_exe_path.trim().is_empty() {
            let path = PathBuf::from(&config.everything_exe_path);
            return if path.is_file() {
                Ok(path)
            } else {
                Err(format!(
                    "Everything executable not found: {}",
                    path.display()
                ))
            };
        }
        let mut candidates = Vec::new();
        if let Ok(exe) = std::env::current_exe()
            && let Some(parent) = exe.parent()
        {
            candidates.push(parent.join("Everything.exe"));
        }
        for variable in [
            "ProgramW6432",
            "ProgramFiles",
            "ProgramFiles(x86)",
            "LOCALAPPDATA",
        ] {
            if let Some(root) = std::env::var_os(variable) {
                for relative in [
                    "Everything/Everything.exe",
                    "Everything 1.5a/Everything.exe",
                    "Programs/Everything/Everything.exe",
                ] {
                    candidates.push(PathBuf::from(&root).join(relative));
                }
            }
        }
        if let Some(paths) = std::env::var_os("PATH") {
            candidates.extend(
                std::env::split_paths(&paths)
                    .filter(|p| p.is_absolute())
                    .map(|p| p.join("Everything.exe")),
            );
        }
        candidates
            .into_iter()
            .find(|path| path.is_file())
            .ok_or_else(|| "Everything not found; set file_search.everything_exe_path".into())
    }

    fn server(config: &FileSearchConfig, deadline: Instant) -> Result<HWND, String> {
        if let Some(hwnd) = discover_window() {
            return Ok(hwnd);
        }
        if !config.auto_spawn_headless {
            return Err("Everything is not running and automatic startup is disabled".into());
        }
        let executable = discover_executable(config)?;
        let mut child = Command::new(&executable)
            .arg("-startup")
            .spawn()
            .map_err(|e| format!("Start {}: {e}", executable.display()))?;
        while Instant::now() < deadline {
            if let Some(hwnd) = discover_window() {
                return Ok(hwnd);
            }
            if let Some(status) = child
                .try_wait()
                .map_err(|e| format!("Wait for Everything startup: {e}"))?
                && !status.success()
            {
                return Err(format!("Everything startup exited with {status}"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err("Timed out waiting for the Everything IPC window".into())
    }

    pub(super) fn query(
        config: &FileSearchConfig,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SearchResult>, String> {
        ipc_query(0, query, limit)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let sender = server(config, deadline)?;
        static CLASS: LazyLock<Result<(), String>> = LazyLock::new(|| unsafe {
            let module = GetModuleHandleW(None).map_err(|e| format!("Get IPC module: {e}"))?;
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: HINSTANCE(module.0),
                lpszClassName: w!("Orbment.EverythingReply"),
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                Err(format!(
                    "Register Everything reply window: {}",
                    windows::core::Error::from_win32()
                ))
            } else {
                Ok(())
            }
        });
        CLASS.clone()?;
        let state = Box::new(Reply {
            limit,
            result: RefCell::new(None),
        });
        let window = unsafe {
            let module = GetModuleHandleW(None).map_err(|e| e.to_string())?;
            ReplyWindow(
                CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    w!("Orbment.EverythingReply"),
                    w!(""),
                    WINDOW_STYLE(0),
                    0,
                    0,
                    0,
                    0,
                    None,
                    None,
                    Some(HINSTANCE(module.0)),
                    Some((&*state as *const Reply).cast::<c_void>()),
                )
                .map_err(|e| format!("Create Everything reply window: {e}"))?,
            )
        };
        unsafe {
            for message in [WM_COPYDATA, WM_DROPFILES, 0x0049] {
                ChangeWindowMessageFilterEx(window.0, message, MSGFLT_ALLOW, None)
                    .map_err(|e| format!("Allow Everything IPC through UIPI: {e}"))?;
            }
        }
        let payload = ipc_query(window.0.0 as usize as u32, query, limit)?;
        let data = COPYDATASTRUCT {
            dwData: 18,
            cbData: payload.len() as u32,
            lpData: payload.as_ptr().cast::<c_void>() as *mut c_void,
        };
        let mut accepted = 0usize;
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(u128::from(u32::MAX)) as u32;
        if remaining == 0 {
            return Err("Everything query timed out".into());
        }
        let sent = unsafe {
            SendMessageTimeoutW(
                sender,
                WM_COPYDATA,
                WPARAM(window.0.0 as usize),
                LPARAM((&data as *const COPYDATASTRUCT) as isize),
                SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
                remaining,
                Some(&mut accepted),
            )
        };
        if sent.0 == 0 {
            return Err(format!(
                "Send Everything query: {}",
                windows::core::Error::from_win32()
            ));
        }
        if accepted == 0 {
            return Err(
                "Everything rejected the IPC query (requires Everything 1.4.1 or later)".into(),
            );
        }
        loop {
            if let Some(result) = state.result.borrow_mut().take() {
                return result;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("Everything query timed out".into());
            }
            unsafe {
                let mut message = MSG::default();
                while PeekMessageW(&mut message, Some(window.0), 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                if state.result.borrow().is_some() {
                    continue;
                }
                let wait = MsgWaitForMultipleObjectsEx(
                    None,
                    remaining.as_millis().min(u128::from(u32::MAX)) as u32,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                );
                if wait.0 == u32::MAX {
                    return Err(format!(
                        "Wait for Everything reply: {}",
                        windows::core::Error::from_win32()
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_combines_quotes_extensions_sizes_dates_and_negation() {
        let query =
            Query::parse("\"annual report\" ext:pdf;docx size:>=1.5mb dm:2024-02-29 !draft")
                .unwrap();
        let time = parse_time("2024-02-29").unwrap().min as i64;
        assert!(
            query
                .score(
                    "Annual Report.pdf",
                    "C:\\docs\\Annual Report.pdf",
                    2 << 20,
                    time,
                    false
                )
                .is_some()
        );
        assert!(
            query
                .score(
                    "Annual Report draft.pdf",
                    "C:\\docs\\Annual Report draft.pdf",
                    2 << 20,
                    time,
                    false
                )
                .is_none()
        );
        assert!(
            query
                .score(
                    "Annual Report.pdf",
                    "C:\\docs\\Annual Report.pdf",
                    100,
                    time,
                    false
                )
                .is_none()
        );
        assert!(Query::parse("dm:2023-02-29").is_err());
        assert!(Query::parse("size:3gb..1gb").is_err());
        assert!(Query::parse("\"unterminated").is_err());
        assert!(Query::parse("size:NaN").is_err());
        assert_eq!(
            parse_time("1970-01-01").unwrap().min,
            116_444_736_000_000_000
        );
    }

    #[test]
    fn wildcard_unicode_and_path_boundaries() {
        assert!(wildcard("*报?.pdf", "年报告.pdf"));
        assert!(!wildcard("*.pdf", "a.pdf.bak"));
        assert!(wildcard("a*b?c", "axybzc"));
        assert!(within_path(&normalized_path("C:/foo/bar.txt"), "c:\\foo"));
        assert!(!within_path("c:\\foobar\\x", "c:\\foo"));
        assert_eq!(normalized_path("\\\\?\\C:\\Foo\\"), "c:\\foo");
        assert_eq!(
            normalized_path("\\\\?\\UNC\\Server\\Share\\"),
            "\\\\server\\share"
        );
    }

    #[test]
    fn builtin_preserves_alias_paths_and_applies_history_and_search_scope() {
        use crate::mft_engine::{MftNode, MftVolume};
        let mut tree = MftTree {
            volumes: Vec::new(),
            nodes: Vec::new(),
            strings: Vec::new(),
        };
        for (name, parent, frn, flags) in [
            ("C:\\", u32::MAX, 5, 1),
            ("alpha.rs", 0, 20, 8),
            ("my-alpha.rs", 0, 20, 8),
        ] {
            let name_offset = tree.strings.len() as u32;
            tree.strings.extend_from_slice(name.as_bytes());
            tree.nodes.push(MftNode {
                file_ref_number: frn,
                logical_size: 100,
                allocated_size: 4096,
                modified_time_ns100: 0,
                parent_index: parent,
                name_offset,
                name_len: name.len() as u16,
                flags,
                reserved: 0,
            });
        }
        tree.volumes.push(MftVolume {
            volume_serial: 9,
            usn_journal_id: 0,
            last_usn: 0,
            root_frn: 5,
            first_node_index: 0,
            node_count: 3,
            drive_letter: b'C' as u16,
            flags: 0,
            reserved: 0,
        });
        let mut config = FileSearchConfig {
            included_drives: vec!["C:".into()],
            ..Default::default()
        };
        let history = HashMap::from([("c:/MY-alpha.rs".into(), 2)]);
        let results = query_builtin(&tree, &config, "alpha ext:rs", 10, &history).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].name, "my-alpha.rs");
        assert_eq!(
            query_builtin(&tree, &config, "alpha", 1, &history)
                .unwrap()
                .len(),
            1
        );
        config.exclude_paths = vec!["C:\\my-alpha.rs".into()];
        assert_eq!(
            query_builtin(&tree, &config, "alpha", 10, &history).unwrap()[0].name,
            "alpha.rs"
        );
        config.included_drives = vec!["D:".into()];
        assert!(
            query_builtin(&tree, &config, "alpha", 10, &history)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn ipc_query_has_fixed_dword_header_and_utf16_payload() {
        let bytes = ipc_query(0x12345678, "中文 *.rs", 17).unwrap();
        assert_eq!(
            u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            0x12345678
        );
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 17);
        assert_eq!(u32::from_le_bytes(bytes[20..24].try_into().unwrap()), 0x14);
        let expected: Vec<_> = "中文 *.rs"
            .encode_utf16()
            .chain(Some(0))
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(&bytes[28..], expected);
        assert!(ipc_query(0, "bad\0query", 1).is_err());
    }

    #[test]
    fn ipc_decodes_results_and_rejects_truncated_offsets() {
        let path = "C:\\资料\\report.txt";
        let utf16: Vec<_> = path.encode_utf16().collect();
        let mut bytes = Vec::new();
        for value in [1u32, 1, 0, IPC_REQUEST_FLAGS, 1, 0, 28, utf16.len() as u32] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        for value in utf16.into_iter().chain(Some(0)) {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&1234i64.to_le_bytes());
        let results = ipc_results(&bytes, 1).unwrap();
        assert_eq!(results[0].path, path);
        assert_eq!(results[0].name, "report.txt");
        assert_eq!(results[0].logical_size, 1234);
        assert!(!results[0].is_directory);
        assert!(ipc_results(&bytes[..bytes.len() - 1], 1).is_err());
        bytes[24..28].copy_from_slice(&4u32.to_le_bytes());
        assert!(ipc_results(&bytes, 1).is_err());
    }
}
