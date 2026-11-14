use crate::{
    memory::status,
    native::{CombineInformation, FileCacheInformation, Handle, NtSetSystemInformation},
};
use anyhow::{Context, Result, ensure};
use orbment_core::{config::MemoryConfig, i18n::tr};
use serde::{Deserialize, Serialize};
use std::{
    mem::size_of,
    sync::{
        Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use windows::{
    Win32::{
        Foundation::{ERROR_NOT_ALL_ASSIGNED, GetLastError, HANDLE, LUID},
        Security::{
            AdjustTokenPrivileges, GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
            SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_ELEVATION, TOKEN_PRIVILEGES,
            TOKEN_QUERY, TokenElevation,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    },
    core::{PCWSTR, w},
};

pub const SUPPORTED_REGIONS_MASK: u32 = 0x3f;
pub const STALL_RISK_MASK: u32 = 0x18;
static CLEAN_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u32)]
pub enum CleanRegion {
    WorkingSet = 1,
    SystemWorkingSet = 2,
    StandbyPriority0 = 4,
    Standby = 8,
    Modified = 16,
    Combine = 32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RegionReport {
    pub region: CleanRegion,
    pub available_before: u64,
    pub available_after: u64,
    pub released_bytes: u64,
    pub ntstatus: i32,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanReport {
    pub regions_mask: u32,
    pub available_before: u64,
    pub available_after: u64,
    pub released_bytes: u64,
    pub elapsed_ms: u64,
    pub regions: Vec<RegionReport>,
}

struct Privileges {
    token: Handle,
    previous: Vec<TOKEN_PRIVILEGES>,
}

impl Privileges {
    fn acquire() -> Result<Self> {
        let mut raw = HANDLE::default();
        unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut raw,
            )
        }
        .context(tr("memory-admin-required"))?;
        let mut privileges = Self {
            token: Handle(raw),
            previous: Vec::new(),
        };
        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned = 0;
        unsafe {
            GetTokenInformation(
                raw,
                TokenElevation,
                Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            )
        }
        .context(tr("memory-admin-required"))?;
        ensure!(
            elevation.TokenIsElevated != 0,
            "{}",
            tr("memory-admin-required")
        );
        for name in [
            w!("SeProfileSingleProcessPrivilege"),
            w!("SeIncreaseQuotaPrivilege"),
        ] {
            privileges.enable(name)?;
        }
        Ok(privileges)
    }

    fn enable(&mut self, name: PCWSTR) -> Result<()> {
        let mut luid = LUID::default();
        unsafe { LookupPrivilegeValueW(None, name, &mut luid) }
            .context(tr("memory-privilege-failed"))?;
        let new = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let mut previous = TOKEN_PRIVILEGES::default();
        let mut returned = 0;
        unsafe {
            AdjustTokenPrivileges(
                self.token.0,
                false,
                Some(&new),
                size_of::<TOKEN_PRIVILEGES>() as u32,
                Some(&mut previous),
                Some(&mut returned),
            )
        }
        .context(tr("memory-privilege-failed"))?;
        ensure!(
            unsafe { GetLastError() } != ERROR_NOT_ALL_ASSIGNED,
            "{}",
            tr("memory-admin-required")
        );
        self.previous.push(previous);
        Ok(())
    }
}

impl Drop for Privileges {
    fn drop(&mut self) {
        for previous in self.previous.iter().rev() {
            unsafe {
                let _ = AdjustTokenPrivileges(self.token.0, false, Some(previous), 0, None, None);
            }
        }
    }
}

/// Blocking operation. Call from a UI worker or queue through MemoryService.
/// Uses native Windows memory-cleaning operations within the six-region supported mask.
pub fn clean(mask: u32) -> Result<CleanReport> {
    ensure!(
        mask != 0 && mask & !SUPPORTED_REGIONS_MASK == 0,
        "{}",
        tr("memory-clean-mask-invalid")
    );
    let _lock = CLEAN_LOCK.lock().expect("memory clean lock");
    let _privileges = Privileges::acquire()?;
    let started = Instant::now();
    let before = status()?.ullAvailPhys;
    let mut regions = Vec::new();
    for region in [
        CleanRegion::WorkingSet,
        CleanRegion::SystemWorkingSet,
        CleanRegion::StandbyPriority0,
        CleanRegion::Standby,
        CleanRegion::Modified,
        CleanRegion::Combine,
    ] {
        if mask & region as u32 == 0 {
            continue;
        }
        let region_before = status()?.ullAvailPhys;
        let ntstatus = match region {
            CleanRegion::SystemWorkingSet => {
                let mut info = FileCacheInformation {
                    minimum_working_set: usize::MAX,
                    maximum_working_set: usize::MAX,
                    ..Default::default()
                };
                unsafe {
                    NtSetSystemInformation(
                        81,
                        (&mut info as *mut FileCacheInformation).cast(),
                        size_of::<FileCacheInformation>() as u32,
                    )
                }
            }
            CleanRegion::Combine => {
                let mut info = CombineInformation::default();
                unsafe {
                    NtSetSystemInformation(
                        130,
                        (&mut info as *mut CombineInformation).cast(),
                        size_of::<CombineInformation>() as u32,
                    )
                }
            }
            _ => {
                let mut command: u32 = match region {
                    CleanRegion::WorkingSet => 2,
                    CleanRegion::Modified => 3,
                    CleanRegion::Standby => 4,
                    CleanRegion::StandbyPriority0 => 5,
                    _ => unreachable!(),
                };
                unsafe {
                    NtSetSystemInformation(
                        80,
                        (&mut command as *mut u32).cast(),
                        size_of::<u32>() as u32,
                    )
                }
            }
        };
        let error = (ntstatus < 0).then(|| {
            format!(
                "{} (NTSTATUS 0x{:08X})",
                tr(
                    if ntstatus == 0xC0000061u32 as i32 || ntstatus == 0xC0000022u32 as i32 {
                        "memory-admin-required"
                    } else {
                        "memory-clean-region-failed"
                    }
                ),
                ntstatus as u32
            )
        });
        let region_after = status()?.ullAvailPhys;
        regions.push(RegionReport {
            region,
            available_before: region_before,
            available_after: region_after,
            released_bytes: region_after.saturating_sub(region_before),
            ntstatus,
            error,
        });
    }
    let after = status()?.ullAvailPhys;
    Ok(CleanReport {
        regions_mask: mask,
        available_before: before,
        available_after: after,
        released_bytes: after.saturating_sub(before),
        elapsed_ms: started.elapsed().as_millis() as u64,
        regions,
    })
}

#[derive(Clone, Debug)]
pub enum MemoryEvent {
    Usage {
        load_pct: u32,
    },
    Cleaned {
        report: CleanReport,
        automatic: bool,
    },
    Failed {
        message: String,
        automatic: bool,
    },
}

enum Command {
    Update(Box<MemoryConfig>),
    Clean(u32),
    Stop,
}

pub struct MemoryService {
    commands: Sender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl MemoryService {
    pub fn start(
        config: MemoryConfig,
        notify: impl Fn(MemoryEvent) + Send + 'static,
    ) -> Result<Self> {
        let (commands, receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("orbment-memory".into())
            .spawn(move || run(config, receiver, notify))
            .context(tr("memory-worker-start"))?;
        Ok(Self {
            commands,
            worker: Some(worker),
        })
    }

    pub fn update(&self, config: MemoryConfig) -> Result<()> {
        self.commands
            .send(Command::Update(Box::new(config)))
            .context(tr("memory-worker-stopped"))
    }

    pub fn clean(&self, mask: u32) -> Result<()> {
        ensure!(
            mask != 0 && mask & !SUPPORTED_REGIONS_MASK == 0,
            "{}",
            tr("memory-clean-mask-invalid")
        );
        self.commands
            .send(Command::Clean(mask))
            .context(tr("memory-worker-stopped"))
    }
}

impl Drop for MemoryService {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Stop);
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::error!("Memory worker panicked");
        }
    }
}

fn deliver(mask: u32, automatic: bool, notify: &impl Fn(MemoryEvent)) {
    match clean(mask) {
        Ok(report) => notify(MemoryEvent::Cleaned { report, automatic }),
        Err(error) => notify(MemoryEvent::Failed {
            message: format!("{error:#}"),
            automatic,
        }),
    }
}

fn run(mut config: MemoryConfig, receiver: Receiver<Command>, notify: impl Fn(MemoryEvent)) {
    let mut last_attempt = Instant::now();
    let mut last_usage = None;
    loop {
        let polling = config.needs_background_service();
        let command = if polling {
            receiver.recv_timeout(Duration::from_secs(1))
        } else {
            receiver
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
        };
        match command {
            Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Command::Update(next)) => {
                config = *next;
                last_attempt = Instant::now();
                last_usage = None;
            }
            Ok(Command::Clean(mask)) => {
                deliver(mask, false, &notify);
                last_attempt = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let memory = match status() {
                    Ok(memory) => memory,
                    Err(error) => {
                        notify(MemoryEvent::Failed {
                            message: format!("{error:#}"),
                            automatic: true,
                        });
                        continue;
                    }
                };
                if last_usage != Some(memory.dwMemoryLoad) {
                    notify(MemoryEvent::Usage {
                        load_pct: memory.dwMemoryLoad,
                    });
                    last_usage = Some(memory.dwMemoryLoad);
                }
                let elapsed = last_attempt.elapsed();
                let threshold = config.auto_clean_threshold_enabled
                    && memory.dwMemoryLoad >= u32::from(config.auto_clean_threshold_pct)
                    && elapsed >= Duration::from_secs(30);
                let interval = config.auto_clean_interval_enabled
                    && elapsed
                        >= Duration::from_secs(u64::from(config.auto_clean_interval_mins) * 60);
                if threshold || interval {
                    deliver(config.clean_regions_mask, true, &notify);
                    last_attempt = Instant::now();
                }
            }
        }
    }
}
