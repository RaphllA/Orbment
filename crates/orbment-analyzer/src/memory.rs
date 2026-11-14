use crate::{
    native::{MemoryListInformation, NtQuerySystemInformation},
    process::{self, ProcessIdentity, ProcessInfo},
};
use anyhow::{Context, Result, ensure};
use orbment_core::i18n::tr;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, mem::size_of};
use windows::Win32::{
    Foundation::{ERROR_INVALID_PARAMETER, GetLastError, WAIT_TIMEOUT},
    System::{
        Memory::{MEM_COMMIT, MEM_IMAGE, MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQueryEx},
        ProcessStatus::{GetPerformanceInfo, K32GetMappedFileNameW, PERFORMANCE_INFORMATION},
        SystemInformation::{
            GetNativeSystemInfo, GlobalMemoryStatusEx, MEMORYSTATUSEX, SYSTEM_INFO,
        },
        Threading::{PROCESS_QUERY_INFORMATION, PROCESS_VM_READ, WaitForSingleObject},
    },
};

pub use crate::memory_clean::{CleanRegion, CleanReport, MemoryEvent, MemoryService, clean};
pub use crate::memory_treemap::{memory_tree, virtual_tree};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MemoryLists {
    pub free_bytes: u64,
    pub standby_bytes: u64,
    pub modified_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProcessGroup {
    pub name: String,
    pub working_set: u64,
    pub private_bytes: u64,
    pub processes: Vec<ProcessInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemorySnapshot {
    pub physical_total: u64,
    pub physical_available: u64,
    pub load_pct: u32,
    pub commit_total: u64,
    pub commit_limit: u64,
    pub virtual_total: u64,
    pub virtual_available: u64,
    pub system_cache: u64,
    pub kernel_paged: u64,
    pub kernel_nonpaged: u64,
    pub process_working_set: u64,
    pub memory_lists: Option<MemoryLists>,
    /// Shared resident pages may be counted in more than one process.
    pub approximate: bool,
    pub processes: Vec<ProcessInfo>,
    pub families: Vec<ProcessGroup>,
    pub inaccessible_processes: usize,
}

pub(crate) fn status() -> Result<MEMORYSTATUSEX> {
    let mut status = MEMORYSTATUSEX {
        dwLength: size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    unsafe { GlobalMemoryStatusEx(&mut status) }.context(tr("memory-status-failed"))?;
    Ok(status)
}

pub fn snapshot() -> Result<MemorySnapshot> {
    let status = status()?;
    let mut performance = PERFORMANCE_INFORMATION::default();
    unsafe {
        GetPerformanceInfo(
            &mut performance,
            size_of::<PERFORMANCE_INFORMATION>() as u32,
        )
    }
    .context(tr("memory-status-failed"))?;
    let page = performance.PageSize as u64;
    let mut lists = MemoryListInformation::default();
    let list_status = unsafe {
        NtQuerySystemInformation(
            80,
            (&mut lists as *mut MemoryListInformation).cast(),
            size_of::<MemoryListInformation>() as u32,
            std::ptr::null_mut(),
        )
    };
    let processes = process::snapshot()?;
    let families = process_families(&processes);
    Ok(MemorySnapshot {
        physical_total: status.ullTotalPhys,
        physical_available: status.ullAvailPhys,
        load_pct: status.dwMemoryLoad,
        commit_total: performance.CommitTotal as u64 * page,
        commit_limit: performance.CommitLimit as u64 * page,
        virtual_total: status.ullTotalVirtual,
        virtual_available: status.ullAvailVirtual,
        system_cache: performance.SystemCache as u64 * page,
        kernel_paged: performance.KernelPaged as u64 * page,
        kernel_nonpaged: performance.KernelNonpaged as u64 * page,
        process_working_set: processes.iter().map(|p| p.working_set).sum(),
        memory_lists: (list_status >= 0).then(|| MemoryLists {
            free_bytes: (lists.zero + lists.free) as u64 * page,
            standby_bytes: lists.standby.iter().sum::<usize>() as u64 * page,
            modified_bytes: (lists.modified + lists.modified_no_write + lists.modified_pagefile)
                as u64
                * page,
        }),
        approximate: true,
        inaccessible_processes: processes.iter().filter(|p| !p.accessible).count(),
        processes,
        families,
    })
}

pub fn overview(top_n: usize) -> Result<MemorySnapshot> {
    ensure!(
        (1..=1000).contains(&top_n),
        "{}",
        tr("memory-top-count-invalid")
    );
    let mut snapshot = snapshot()?;
    snapshot
        .processes
        .sort_by_key(|p| std::cmp::Reverse(p.working_set));
    snapshot.processes.truncate(top_n);
    // Overview is bounded; the dedicated snapshot API supplies complete tree data.
    snapshot
        .families
        .sort_by_key(|f| std::cmp::Reverse(f.working_set));
    snapshot.families.truncate(top_n);
    for family in &mut snapshot.families {
        family
            .processes
            .sort_by_key(|p| std::cmp::Reverse(p.working_set));
        family.processes.truncate(top_n);
    }
    Ok(snapshot)
}

pub fn process_families(processes: &[ProcessInfo]) -> Vec<ProcessGroup> {
    fn root(parents: &[usize], mut i: usize) -> usize {
        while parents[i] != i {
            i = parents[i];
        }
        i
    }
    fn directory(path: &str) -> &str {
        path.rsplit_once(['\\', '/']).map_or("", |(d, _)| d)
    }
    let mut parents: Vec<usize> = (0..processes.len()).collect();
    let mut names = BTreeMap::new();
    let pids: BTreeMap<u32, usize> = processes
        .iter()
        .enumerate()
        .map(|(i, p)| (p.pid, i))
        .collect();
    for (i, p) in processes.iter().enumerate() {
        if let Some(other) = names.insert(p.name.to_lowercase(), i) {
            let a = root(&parents, i);
            let b = root(&parents, other);
            parents[a] = b;
        }
        if let Some(&parent_index) = pids.get(&p.parent_pid) {
            let parent = &processes[parent_index];
            // A shared installation directory avoids grouping every desktop app under Explorer.
            if process::valid_parent(p, parent)
                && matches!((&p.path, &parent.path), (Some(a), Some(b)) if directory(a).eq_ignore_ascii_case(directory(b)))
            {
                let a = root(&parents, i);
                let b = root(&parents, parent_index);
                parents[a] = b;
            }
        }
    }
    let mut groups: BTreeMap<usize, ProcessGroup> = BTreeMap::new();
    for (i, p) in processes.iter().enumerate() {
        let root = root(&parents, i);
        let group = groups.entry(root).or_insert_with(|| ProcessGroup {
            name: processes[root].name.clone(),
            working_set: 0,
            private_bytes: 0,
            processes: Vec::new(),
        });
        group.working_set += p.working_set;
        group.private_bytes += p.private_bytes;
        group.processes.push(p.clone());
    }
    let mut groups: Vec<_> = groups.into_values().collect();
    groups.sort_by_key(|g| std::cmp::Reverse(g.working_set));
    groups
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum VirtualKind {
    Image,
    MappedFile,
    PrivateCommit,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VirtualRegion {
    pub base_address: u64,
    pub allocation_base: u64,
    pub bytes: u64,
    pub protection: u32,
    pub kind: VirtualKind,
    /// Native device path as returned by GetMappedFileNameW.
    pub mapped_path: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VirtualMemorySnapshot {
    pub identity: ProcessIdentity,
    pub committed_bytes: u64,
    pub regions: Vec<VirtualRegion>,
}

pub fn virtual_regions(identity: ProcessIdentity) -> Result<VirtualMemorySnapshot> {
    let handle = process::open_verified(identity, PROCESS_QUERY_INFORMATION | PROCESS_VM_READ)?;
    let mut system = SYSTEM_INFO::default();
    unsafe {
        GetNativeSystemInfo(&mut system);
    }
    let mut address = system.lpMinimumApplicationAddress as usize;
    let maximum = system.lpMaximumApplicationAddress as usize;
    let mut regions = Vec::new();
    let mut mapped_paths = BTreeMap::new();
    while address <= maximum {
        let mut region = MEMORY_BASIC_INFORMATION::default();
        let read = unsafe {
            VirtualQueryEx(
                handle.0,
                Some(address as *const _),
                &mut region,
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if read == 0 {
            let error = unsafe { GetLastError() };
            // A 32-bit target ends before this 64-bit caller's maximum user address.
            ensure!(
                error == ERROR_INVALID_PARAMETER,
                "{}: {}",
                tr("memory-virtual-query"),
                error.0
            );
            break;
        }
        if region.State == MEM_COMMIT {
            let kind = if region.Type == MEM_IMAGE {
                VirtualKind::Image
            } else if region.Type == MEM_MAPPED {
                VirtualKind::MappedFile
            } else {
                VirtualKind::PrivateCommit
            };
            let mapped_path = if kind == VirtualKind::PrivateCommit {
                None
            } else {
                mapped_paths
                    .entry(region.AllocationBase as usize)
                    .or_insert_with(|| {
                        let mut path = vec![0u16; 32768];
                        let count = unsafe {
                            K32GetMappedFileNameW(handle.0, region.BaseAddress, &mut path)
                        } as usize;
                        (count > 0).then(|| String::from_utf16_lossy(&path[..count]))
                    })
                    .clone()
            };
            regions.push(VirtualRegion {
                base_address: region.BaseAddress as u64,
                allocation_base: region.AllocationBase as u64,
                bytes: region.RegionSize as u64,
                protection: region.Protect.0,
                kind,
                mapped_path,
            });
        }
        let Some(next) = (region.BaseAddress as usize).checked_add(region.RegionSize) else {
            break;
        };
        ensure!(next > address, "{}", tr("memory-virtual-query"));
        address = next;
    }
    ensure!(
        unsafe { WaitForSingleObject(handle.0, 0) } == WAIT_TIMEOUT,
        "{}",
        tr("memory-process-changed")
    );
    Ok(VirtualMemorySnapshot {
        identity,
        committed_bytes: regions.iter().map(|r| r.bytes).sum(),
        regions,
    })
}
