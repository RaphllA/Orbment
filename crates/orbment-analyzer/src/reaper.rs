use crate::process::{self, ProcessIdentity, ProcessInfo};
use anyhow::{Context, Result, ensure};
use orbment_core::{
    config::{ProcessFamily, ReaperConfig},
    i18n::tr,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, WAIT_OBJECT_0, WPARAM},
        System::Threading::{TerminateProcess, WaitForSingleObject},
        UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, PostMessageW, WM_CLOSE},
    },
    core::BOOL,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReapFamily {
    pub family_index: usize,
    pub family: ProcessFamily,
    pub processes: Vec<ProcessInfo>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReapPreview {
    pub families: Vec<ReapFamily>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReapMode {
    Graceful,
    Force,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReapOutcome {
    Exited,
    Terminated,
    CloseRequested,
    NoWindow,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReapResult {
    pub identity: ProcessIdentity,
    pub outcome: ReapOutcome,
    pub windows_notified: usize,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ReapReport {
    pub processes: Vec<ReapResult>,
    pub aborted: Option<String>,
}

fn named(names: &[String], process: &ProcessInfo) -> bool {
    names
        .iter()
        .any(|name| name.eq_ignore_ascii_case(&process.name))
}

fn path_matches(path: &str, prefix: &str) -> bool {
    let path = path.replace('/', "\\").to_lowercase();
    let prefix = prefix
        .replace('/', "\\")
        .trim_end_matches('\\')
        .to_lowercase();
    path == prefix
        || path
            .strip_prefix(&prefix)
            .is_some_and(|rest| rest.starts_with('\\'))
}

fn owner_running(family: &ProcessFamily, processes: &[ProcessInfo]) -> bool {
    processes.iter().any(|p| {
        named(&family.owners, p)
            && family.path_prefix.as_ref().is_none_or(|prefix| {
                // An unreadable matching owner is a veto, never evidence that it exited.
                p.path
                    .as_ref()
                    .is_none_or(|path| path_matches(path, prefix))
            })
    })
}

fn allowed(family: &ProcessFamily, process: &ProcessInfo, own_pid: u32) -> bool {
    process.pid > 4
        && process.pid != own_pid
        && process.identity.is_some()
        && !named(&family.exemptions, process)
        && !named(&family.owners, process)
}

pub fn preview(config: &ReaperConfig) -> Result<ReapPreview> {
    Ok(preview_from_processes(
        config,
        &process::snapshot()?,
        std::process::id(),
    ))
}

pub fn preview_from_processes(
    config: &ReaperConfig,
    processes: &[ProcessInfo],
    own_pid: u32,
) -> ReapPreview {
    let by_pid = process::by_pid(processes);
    let mut families = Vec::new();
    for (family_index, family) in config.families.iter().enumerate() {
        if owner_running(family, processes) {
            continue;
        }
        let mut candidates: BTreeSet<u32> = processes
            .iter()
            .filter(|p| {
                allowed(family, p, own_pid)
                    && named(&family.companions, p)
                    && family.path_prefix.as_ref().is_none_or(|prefix| {
                        p.path
                            .as_ref()
                            .is_some_and(|path| path_matches(path, prefix))
                    })
            })
            .map(|p| p.pid)
            .collect();
        if family.include_descendants {
            loop {
                let before = candidates.len();
                for p in processes {
                    if allowed(family, p, own_pid)
                        && candidates.contains(&p.parent_pid)
                        && by_pid
                            .get(&p.parent_pid)
                            .is_some_and(|parent| process::valid_parent(p, parent))
                    {
                        candidates.insert(p.pid);
                    }
                }
                if candidates.len() == before {
                    break;
                }
            }
        }
        if !candidates.is_empty() {
            families.push(ReapFamily {
                family_index,
                family: family.clone(),
                processes: processes
                    .iter()
                    .filter(|p| candidates.contains(&p.pid))
                    .cloned()
                    .collect(),
            });
        }
    }
    ReapPreview { families }
}

fn same_family(a: &ProcessFamily, b: &ProcessFamily) -> bool {
    a.name == b.name
        && a.owners == b.owners
        && a.companions == b.companions
        && a.exemptions == b.exemptions
        && a.path_prefix == b.path_prefix
        && a.include_descendants == b.include_descendants
}

fn selected_families(
    config: &ReaperConfig,
    preview: &ReapPreview,
    targets: &[ProcessIdentity],
) -> Result<BTreeSet<usize>> {
    ensure!(
        !targets.is_empty(),
        "{}",
        tr("memory-reaper-selection-invalid")
    );
    let mut selected = BTreeSet::new();
    let mut identities = BTreeSet::new();
    for target in targets {
        ensure!(
            target.creation_time != 0 && target.pid > 4 && identities.insert(*target),
            "{}",
            tr("memory-reaper-selection-invalid")
        );
        let mut found = false;
        for family in &preview.families {
            if family
                .processes
                .iter()
                .any(|p| p.pid == target.pid && p.identity == Some(*target))
            {
                ensure!(
                    config
                        .families
                        .get(family.family_index)
                        .is_some_and(|current| same_family(current, &family.family)),
                    "{}",
                    tr("memory-reaper-preview-stale")
                );
                selected.insert(family.family_index);
                found = true;
            }
        }
        ensure!(found, "{}", tr("memory-reaper-selection-invalid"));
    }
    Ok(selected)
}

fn validate_current(
    config: &ReaperConfig,
    preview: &ReapPreview,
    targets: &[ProcessIdentity],
    processes: &[ProcessInfo],
    own_pid: u32,
) -> Result<BTreeSet<usize>> {
    let families = selected_families(config, preview, targets)?;
    ensure!(
        !families
            .iter()
            .any(|i| owner_running(&config.families[*i], processes)),
        "{}",
        tr("memory-reaper-owner-running")
    );
    let fresh = preview_from_processes(config, processes, own_pid);
    for target in targets {
        // Match every originally selected family, not merely any new family with the same PID.
        for original in &preview.families {
            if original
                .processes
                .iter()
                .any(|p| p.identity == Some(*target))
            {
                ensure!(
                    fresh
                        .families
                        .iter()
                        .any(|f| f.family_index == original.family_index
                            && f.processes.iter().any(|p| p.identity == Some(*target))),
                    "{}",
                    tr("memory-reaper-preview-stale")
                );
            }
        }
    }
    Ok(families)
}

/// Caller must separately confirm Force. No automatic escalation from WM_CLOSE ever occurs.
pub fn execute(
    config: &ReaperConfig,
    preview: &ReapPreview,
    targets: &[ProcessIdentity],
    mode: ReapMode,
) -> Result<ReapReport> {
    execute_with_cancel(config, preview, targets, mode, &AtomicBool::new(false))
}

/// Cancellation stops further actions and waiting; already posted WM_CLOSE messages remain posted.
pub fn execute_with_cancel(
    config: &ReaperConfig,
    preview: &ReapPreview,
    targets: &[ProcessIdentity],
    mode: ReapMode,
    cancel: &AtomicBool,
) -> Result<ReapReport> {
    if cancel.load(Ordering::Acquire) {
        return Ok(ReapReport::default());
    }
    // Acquire all handles before any side effect; the live generation remains pinned throughout.
    let handles = targets
        .iter()
        .map(|identity| process::open_destructive(*identity, mode == ReapMode::Force))
        .collect::<Result<Vec<_>>>()?;
    let families = validate_current(
        config,
        preview,
        targets,
        &process::snapshot()?,
        std::process::id(),
    )?;
    let mut report = ReapReport::default();
    for (identity, handle) in targets.iter().zip(&handles) {
        if cancel.load(Ordering::Acquire) {
            break;
        }
        let current = process::snapshot()?;
        if families
            .iter()
            .any(|i| owner_running(&config.families[*i], &current))
        {
            report.aborted = Some(tr("memory-reaper-owner-running"));
            break;
        }
        if unsafe { WaitForSingleObject(handle.0, 0) } == WAIT_OBJECT_0 {
            report.processes.push(ReapResult {
                identity: *identity,
                outcome: ReapOutcome::Exited,
                windows_notified: 0,
                error: None,
            });
            continue;
        }
        let mut result = ReapResult {
            identity: *identity,
            outcome: ReapOutcome::Failed,
            windows_notified: 0,
            error: None,
        };
        if cancel.load(Ordering::Acquire) {
            break;
        }
        match mode {
            ReapMode::Force => match unsafe { TerminateProcess(handle.0, 1) } {
                Ok(()) => result.outcome = ReapOutcome::Terminated,
                Err(error) => {
                    result.error = Some(format!("{}: {error}", tr("memory-process-kill")))
                }
            },
            ReapMode::Graceful => {
                let mut windows = ProcessWindows {
                    pid: identity.pid,
                    windows: Vec::new(),
                };
                unsafe {
                    EnumWindows(
                        Some(collect_windows),
                        LPARAM((&mut windows as *mut ProcessWindows) as isize),
                    )
                }
                .context(tr("memory-process-windows"))?;
                for window in windows.windows {
                    if cancel.load(Ordering::Acquire) {
                        break;
                    }
                    let mut pid = 0;
                    unsafe {
                        GetWindowThreadProcessId(window, Some(&mut pid));
                    }
                    if pid != identity.pid
                        || unsafe { WaitForSingleObject(handle.0, 0) } == WAIT_OBJECT_0
                    {
                        continue;
                    }
                    match unsafe { PostMessageW(Some(window), WM_CLOSE, WPARAM(0), LPARAM(0)) } {
                        Ok(()) => result.windows_notified += 1,
                        Err(error) => {
                            result.error = Some(format!("{}: {error}", tr("memory-process-close")))
                        }
                    }
                }
                result.outcome = if result.windows_notified > 0 {
                    ReapOutcome::CloseRequested
                } else if result.error.is_some() {
                    ReapOutcome::Failed
                } else {
                    ReapOutcome::NoWindow
                };
            }
        }
        report.processes.push(result);
    }
    if mode == ReapMode::Graceful {
        let deadline =
            Instant::now() + Duration::from_secs(u64::from(config.close_timeout_seconds));
        wait_for_close(deadline, cancel, || {
            let mut waiting = false;
            for (result, handle) in report.processes.iter_mut().zip(&handles) {
                if result.outcome == ReapOutcome::CloseRequested {
                    if unsafe { WaitForSingleObject(handle.0, 0) } == WAIT_OBJECT_0 {
                        result.outcome = ReapOutcome::Exited;
                    } else {
                        waiting = true;
                    }
                }
            }
            waiting
        });
    }
    Ok(report)
}

fn wait_for_close(deadline: Instant, cancel: &AtomicBool, mut waiting: impl FnMut() -> bool) {
    while !cancel.load(Ordering::Acquire) && waiting() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct ProcessWindows {
    pid: u32,
    windows: Vec<HWND>,
}
unsafe extern "system" fn collect_windows(window: HWND, parameter: LPARAM) -> BOOL {
    let state = unsafe { &mut *(parameter.0 as *mut ProcessWindows) };
    let mut pid = 0;
    unsafe {
        GetWindowThreadProcessId(window, Some(&mut pid));
    }
    if pid == state.pid {
        state.windows.push(window);
    }
    BOOL(1)
}

pub fn prefill_family(
    processes: &[ProcessInfo],
    selected: ProcessIdentity,
) -> Result<ProcessFamily> {
    let owner = processes
        .iter()
        .find(|p| p.identity == Some(selected))
        .context(tr("memory-process-changed"))?;
    let mut family = ProcessFamily {
        name: owner.name.clone(),
        owners: vec![owner.name.clone()],
        companions: Vec::new(),
        exemptions: Vec::new(),
        path_prefix: None,
        include_descendants: true,
    };
    let mut descendants = BTreeSet::from([owner.pid]);
    loop {
        let before = descendants.len();
        for p in processes {
            if descendants.contains(&p.parent_pid)
                && processes.iter().any(|parent| {
                    descendants.contains(&parent.pid) && process::valid_parent(p, parent)
                })
            {
                descendants.insert(p.pid);
                if !p.name.eq_ignore_ascii_case(&owner.name) && !named(&family.companions, p) {
                    family.companions.push(p.name.clone());
                }
            }
        }
        if descendants.len() == before {
            break;
        }
    }
    Ok(family)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_stops_graceful_polling_and_prevents_new_actions() {
        let cancel = AtomicBool::new(false);
        let mut polls = 0;
        wait_for_close(Instant::now() + Duration::from_secs(60), &cancel, || {
            polls += 1;
            cancel.store(true, Ordering::Release);
            true
        });
        assert_eq!(polls, 1);
        let report = execute_with_cancel(
            &config(),
            &ReapPreview::default(),
            &[ProcessIdentity { pid: 0, creation_time: 0 }],
            ReapMode::Force,
            &cancel,
        ).unwrap();
        assert!(report.processes.is_empty());
    }

    fn process(pid: u32, parent_pid: u32, created: u64, name: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid,
            name: name.into(),
            identity: Some(ProcessIdentity {
                pid,
                creation_time: created,
            }),
            ..Default::default()
        }
    }
    fn config() -> ReaperConfig {
        ReaperConfig {
            families: vec![ProcessFamily {
                name: "test".into(),
                owners: vec!["owner.exe".into()],
                companions: vec!["helper.exe".into()],
                exemptions: vec!["exempt.exe".into()],
                path_prefix: None,
                include_descendants: true,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn owner_veto_is_case_insensitive_and_unreadable_path_is_not_absence() {
        let mut config = config();
        config.families[0].path_prefix = Some("C:\\App".into());
        let mut helper = process(20, 10, 20, "helper.exe");
        helper.path = Some("C:\\App\\helper.exe".into());
        let owner = process(10, 0, 10, "OWNER.EXE");
        assert!(
            preview_from_processes(&config, &[owner, helper], 99)
                .families
                .is_empty()
        );
        assert!(!path_matches("C:\\Application\\helper.exe", "C:\\App"));
    }

    #[test]
    fn descendants_require_live_parent_generation_and_exemptions_block_branch() {
        let processes = vec![
            process(20, 0, 100, "helper.exe"),
            process(21, 20, 101, "child.exe"),
            process(22, 20, 50, "reused.exe"),
            process(23, 20, 102, "exempt.exe"),
            process(24, 23, 103, "grandchild.exe"),
            process(4, 20, 101, "helper.exe"),
            process(99, 20, 101, "helper.exe"),
        ];
        let preview = preview_from_processes(&config(), &processes, 99);
        assert_eq!(
            preview.families[0]
                .processes
                .iter()
                .map(|p| p.pid)
                .collect::<Vec<_>>(),
            vec![20, 21]
        );
    }

    #[test]
    fn execution_validation_rejects_owner_restart_pid_reuse_and_selection_changes() {
        let config = config();
        let helper = process(20, 0, 100, "helper.exe");
        let preview = preview_from_processes(&config, std::slice::from_ref(&helper), 99);
        let target = helper.identity.unwrap();
        assert!(
            validate_current(
                &config,
                &preview,
                &[target],
                std::slice::from_ref(&helper),
                99
            )
            .is_ok()
        );
        assert!(
            validate_current(
                &config,
                &preview,
                &[target],
                &[helper.clone(), process(10, 0, 200, "owner.exe")],
                99
            )
            .is_err()
        );
        assert!(
            validate_current(
                &config,
                &preview,
                &[target],
                &[process(20, 0, 300, "helper.exe")],
                99
            )
            .is_err()
        );
        assert!(selected_families(&config, &preview, &[]).is_err());
        assert!(selected_families(&config, &preview, &[target, target]).is_err());
        let mut changed = config.clone();
        changed.families[0].include_descendants = false;
        assert!(selected_families(&changed, &preview, &[target]).is_err());
    }
}
