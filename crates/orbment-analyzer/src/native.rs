use std::ffi::c_void;
use windows::Win32::Foundation::{CloseHandle, HANDLE};

pub(crate) struct Handle(pub HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

#[link(name = "ntdll")]
unsafe extern "system" {
    pub(crate) fn NtQuerySystemInformation(
        class: u32,
        buffer: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
    pub(crate) fn NtSetSystemInformation(class: u32, buffer: *mut c_void, length: u32) -> i32;
    pub(crate) fn NtQueryInformationProcess(
        process: HANDLE,
        class: u32,
        buffer: *mut c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
}

#[repr(C)]
pub(crate) struct UnicodeString {
    pub length: u16,
    pub maximum_length: u16,
    pub buffer: *const u16,
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct ProcessBasicInformation {
    pub exit_status: i32,
    pub peb_address: usize,
    pub affinity_mask: usize,
    pub base_priority: i32,
    pub pid: usize,
    pub parent_pid: usize,
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct MemoryListInformation {
    pub zero: usize,
    pub free: usize,
    pub modified: usize,
    pub modified_no_write: usize,
    pub bad: usize,
    pub standby: [usize; 8],
    pub repurposed: [usize; 8],
    pub modified_pagefile: usize,
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct FileCacheInformation {
    pub current_size: usize,
    pub peak_size: usize,
    pub page_fault_count: u32,
    pub minimum_working_set: usize,
    pub maximum_working_set: usize,
    pub current_size_including_transition: usize,
    pub peak_size_including_transition: usize,
    pub transition_repurpose_count: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Default)]
pub(crate) struct CombineInformation {
    pub handle: HANDLE,
    pub pages_combined: usize,
    pub flags: u32,
}
