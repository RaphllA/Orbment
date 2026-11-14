use std::{
    collections::BTreeMap,
    ffi::c_void,
    mem::{size_of, size_of_val},
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use orbment_core::config::MonitorConfig;
use serde::Serialize;
use windows::Wdk::Graphics::Direct3D::*;
use windows::{
    Win32::{
        Foundation::{CloseHandle, FreeLibrary, HANDLE, HMODULE},
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
            QueryDosDeviceW,
        },
        System::{
            IO::DeviceIoControl,
            Ioctl::*,
            LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW},
            Performance::*,
            Power::*,
            SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
            Threading::GetActiveProcessorCount,
        },
    },
    core::{BOOL, PCSTR, PCWSTR},
};

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SensorSnapshot {
    pub cpu_percent: Option<f64>,
    pub memory_percent: Option<f64>,
    pub memory_total_bytes: Option<u64>,
    pub memory_available_bytes: Option<u64>,
    pub gpu_percent: Option<f64>,
    pub gpu_dedicated_bytes: Option<u64>,
    pub disk_percent: Option<f64>,
    pub disk_read_bytes_per_second: Option<f64>,
    pub disk_write_bytes_per_second: Option<f64>,
    pub cpu_frequency_mhz: Option<f64>,
    pub cpu_temperature_c: Option<f64>,
    pub gpu_temperature_c: Option<f64>,
    pub disk_temperature_c: Option<f64>,
    /// Localization keys, including actionable PawnIO installation/module status.
    pub unavailable: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SensorDemand {
    pub cpu_pdh: bool,
    pub memory: bool,
    pub gpu: bool,
    pub disk: bool,
    pub cpu_frequency: bool,
    pub cpu_temperature: bool,
    pub gpu_temperature: bool,
    pub disk_temperature: bool,
}

impl SensorDemand {
    /// Skin callers may include additional bits used by their currently visible layout.
    pub fn from_config(config: &MonitorConfig, skin_mask: u32) -> Self {
        let mask = if config.taskbar.enabled {
            config.taskbar.display_mask
        } else {
            0
        } | skin_mask;
        let hardware = config.hardware_monitor_mask;
        Self {
            cpu_pdh: !config.cpu_usage_by_times
                && (mask & 4 != 0 || config.floating.show_cpu_memory),
            memory: mask & 8 != 0
                || (config.floating.enabled && config.floating.show_cpu_memory)
                || config.alerts.memory_enabled,
            gpu: hardware & 2 != 0 && mask & 16 != 0,
            disk: hardware & 4 != 0 && mask & (512 | 8192 | 16384) != 0,
            cpu_frequency: hardware & 1 != 0 && mask & 2048 != 0,
            cpu_temperature: hardware & 1 != 0
                && (mask & 32 != 0 || config.alerts.cpu_temperature_enabled),
            gpu_temperature: hardware & 2 != 0
                && (mask & 64 != 0 || config.alerts.gpu_temperature_enabled),
            disk_temperature: hardware & 4 != 0
                && (mask & 128 != 0 || config.alerts.disk_temperature_enabled),
        }
    }
}

/// Construct and use on the worker thread; no sensor libraries/handles exist when disabled.
pub struct Sensors {
    demand: SensorDemand,
    counters: Option<Counters>,
    nvml: Option<Nvml>,
    adl: Option<Adl>,
    kmt: Option<KmtAdapters>,
    pawn: Option<Pawn>,
    pawn_error: Option<&'static str>,
    disks: Vec<Disk>,
    disk_temperature: Option<f64>,
    next_disk_poll: Instant,
    core: String,
}

impl Sensors {
    pub fn new(config: &MonitorConfig, demand: SensorDemand, module_dir: &Path) -> Self {
        let counters = if demand.cpu_pdh || demand.cpu_temperature || demand.gpu || demand.disk {
            Counters::new(config, demand)
                .map_err(|error| tracing::warn!(%error, "Opening hardware performance counters"))
                .ok()
        } else {
            None
        };
        let (pawn, pawn_error) = if demand.cpu_temperature {
            match Pawn::open(module_dir) {
                Ok(pawn) => (Some(pawn), None),
                Err(error) => (None, Some(error)),
            }
        } else {
            (None, None)
        };
        Self {
            demand,
            counters,
            nvml: if demand.gpu_temperature {
                Nvml::open().ok()
            } else {
                None
            },
            adl: if demand.gpu_temperature {
                Adl::open().ok()
            } else {
                None
            },
            kmt: if demand.gpu_temperature {
                KmtAdapters::open().ok()
            } else {
                None
            },
            pawn,
            pawn_error,
            disks: if demand.disk_temperature {
                Disk::enumerate(&config.hard_disk_name)
            } else {
                Vec::new()
            },
            disk_temperature: None,
            next_disk_poll: Instant::now(),
            core: config.cpu_core_name.clone(),
        }
    }

    pub fn sample(&mut self) -> SensorSnapshot {
        let mut result = SensorSnapshot::default();
        if self.demand.memory {
            let mut memory = MEMORYSTATUSEX {
                dwLength: size_of::<MEMORYSTATUSEX>() as u32,
                ..Default::default()
            };
            if unsafe { GlobalMemoryStatusEx(&mut memory) }.is_ok() {
                result.memory_percent = Some(f64::from(memory.dwMemoryLoad));
                result.memory_total_bytes = Some(memory.ullTotalPhys);
                result.memory_available_bytes = Some(memory.ullAvailPhys);
            }
        }
        if let Some(counters) = &mut self.counters {
            counters.sample(&mut result);
        }
        if self.demand.cpu_frequency {
            result.cpu_frequency_mhz = cpu_frequency(&self.core);
        }
        if self.demand.cpu_temperature {
            self.sample_cpu_temperature(&mut result);
        }
        if self.demand.gpu_temperature {
            result.gpu_temperature_c = self
                .nvml
                .as_ref()
                .and_then(Nvml::temperature)
                .into_iter()
                .chain(self.adl.as_ref().and_then(Adl::temperature))
                .chain(self.kmt.as_ref().and_then(KmtAdapters::temperature))
                .reduce(f64::max);
        }
        if self.demand.disk_temperature {
            if Instant::now() >= self.next_disk_poll {
                self.disk_temperature = self
                    .disks
                    .iter()
                    .filter_map(Disk::temperature)
                    .reduce(f64::max);
                self.next_disk_poll = Instant::now() + Duration::from_secs(15);
            }
            result.disk_temperature_c = self.disk_temperature;
        }
        for (enabled, name, value) in [
            (self.demand.gpu, "gpu", result.gpu_percent),
            (self.demand.disk, "disk", result.disk_percent),
            (
                self.demand.cpu_frequency,
                "cpu_frequency",
                result.cpu_frequency_mhz,
            ),
            (
                self.demand.gpu_temperature,
                "gpu_temperature",
                result.gpu_temperature_c,
            ),
            (
                self.demand.disk_temperature,
                "disk_temperature",
                result.disk_temperature_c,
            ),
        ] {
            if enabled && value.is_none() {
                result
                    .unavailable
                    .insert(name.into(), "monitor-sensor-unavailable".into());
            }
        }
        result
    }

    fn sample_cpu_temperature(&self, result: &mut SensorSnapshot) {
        result.cpu_temperature_c = self
            .pawn
            .as_ref()
            .and_then(Pawn::temperature)
            .or(result.cpu_temperature_c);
        if result.cpu_temperature_c.is_none() {
            result.unavailable.insert(
                "cpu_temperature".into(),
                self.pawn_error
                    .unwrap_or("monitor-sensor-unavailable")
                    .into(),
            );
        }
    }
}

struct Counters {
    query: PDH_HQUERY,
    cpu: Option<PDH_HCOUNTER>,
    cpu_temperature: Option<PDH_HCOUNTER>,
    gpu: Option<PDH_HCOUNTER>,
    vram: Option<PDH_HCOUNTER>,
    disk: Option<PDH_HCOUNTER>,
    disk_read: Option<PDH_HCOUNTER>,
    disk_write: Option<PDH_HCOUNTER>,
}

impl Counters {
    fn new(config: &MonitorConfig, demand: SensorDemand) -> Result<Self> {
        let mut query = PDH_HQUERY::default();
        ensure!(
            unsafe { PdhOpenQueryW(PCWSTR::null(), 0, &mut query) } == 0,
            "PdhOpenQueryW"
        );
        let mut result = Self {
            query,
            cpu: None,
            cpu_temperature: None,
            gpu: None,
            vram: None,
            disk: None,
            disk_read: None,
            disk_write: None,
        };
        if demand.cpu_pdh {
            let core = if config.cpu_core_name == "Core Average" {
                "_Total"
            } else {
                &config.cpu_core_name
            };
            result.cpu = result.add(&format!(r"\Processor({core})\% Processor Time"));
        }
        if demand.cpu_temperature {
            result.cpu_temperature = result
                .add(r"\Thermal Zone Information(*)\High Precision Temperature")
                .or_else(|| result.add(r"\Thermal Zone Information(*)\Temperature"));
        }
        if demand.gpu {
            // Wildcard handles let PDH track changing process instances without re-enumeration.
            result.gpu = result.add(r"\GPU Engine(*)\Utilization Percentage");
            result.vram = result.add(r"\GPU Adapter Memory(*)\Dedicated Usage");
        }
        if demand.disk {
            let disk = if config.hard_disk_name.is_empty() {
                "_Total"
            } else {
                &config.hard_disk_name
            };
            result.disk = result.add(&format!(r"\PhysicalDisk({disk})\% Disk Time"));
            result.disk_read = result.add(&format!(r"\PhysicalDisk({disk})\Disk Read Bytes/sec"));
            result.disk_write = result.add(&format!(r"\PhysicalDisk({disk})\Disk Write Bytes/sec"));
        }
        unsafe {
            PdhCollectQueryData(query);
        }
        Ok(result)
    }

    fn add(&self, path: &str) -> Option<PDH_HCOUNTER> {
        let path = wide(path);
        let mut counter = PDH_HCOUNTER::default();
        (unsafe { PdhAddEnglishCounterW(self.query, PCWSTR(path.as_ptr()), 0, &mut counter) } == 0)
            .then_some(counter)
    }

    fn sample(&mut self, result: &mut SensorSnapshot) {
        if unsafe { PdhCollectQueryData(self.query) } != 0 {
            return;
        }
        result.cpu_percent = self
            .cpu
            .and_then(counter_value)
            .map(|v| v.clamp(0.0, 100.0));
        result.cpu_temperature_c = self
            .cpu_temperature
            .and_then(counter_array)
            .and_then(|rows| {
                rows.into_iter()
                    .filter_map(|(_, value)| thermal_temperature(value))
                    .reduce(f64::max)
            });
        result.disk_percent = self
            .disk
            .and_then(counter_value)
            .map(|v| v.clamp(0.0, 100.0));
        result.disk_read_bytes_per_second = self.disk_read.and_then(counter_value);
        result.disk_write_bytes_per_second = self.disk_write.and_then(counter_value);
        result.gpu_percent = self
            .gpu
            .and_then(counter_array)
            .and_then(|rows| gpu_utilization(&rows));
        result.gpu_dedicated_bytes = self.vram.and_then(counter_array).and_then(|rows| {
            (!rows.is_empty()).then(|| rows.iter().map(|(_, value)| *value as u64).sum())
        });
    }
}

impl Drop for Counters {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

fn counter_value(counter: PDH_HCOUNTER) -> Option<f64> {
    let mut value = PDH_FMT_COUNTERVALUE::default();
    let status = unsafe { PdhGetFormattedCounterValue(counter, PDH_FMT_DOUBLE, None, &mut value) };
    if status != 0 || value.CStatus > 1 {
        return None;
    }
    Some(unsafe { value.Anonymous.doubleValue })
}

fn counter_array(counter: PDH_HCOUNTER) -> Option<Vec<(String, f64)>> {
    let mut bytes = 0;
    let mut count = 0;
    let status = unsafe {
        PdhGetFormattedCounterArrayW(counter, PDH_FMT_DOUBLE, &mut bytes, &mut count, None)
    };
    if status != PDH_MORE_DATA || bytes == 0 {
        return None;
    }
    // PDH writes aligned structs followed by variable UTF-16 names into this buffer.
    let mut buffer = vec![0u64; (bytes as usize).div_ceil(8)];
    let status = unsafe {
        PdhGetFormattedCounterArrayW(
            counter,
            PDH_FMT_DOUBLE,
            &mut bytes,
            &mut count,
            Some(buffer.as_mut_ptr().cast()),
        )
    };
    if status != 0 {
        return None;
    }
    let items = unsafe {
        std::slice::from_raw_parts(
            buffer.as_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
            count as usize,
        )
    };
    Some(
        items
            .iter()
            .filter(|item| item.FmtValue.CStatus <= 1)
            .map(|item| unsafe {
                (
                    item.szName.to_string().unwrap_or_default(),
                    item.FmtValue.Anonymous.doubleValue,
                )
            })
            .collect(),
    )
}

fn thermal_temperature(value: f64) -> Option<f64> {
    let celsius = if value > 2000.0 {
        value / 10.0 - 273.15
    } else if value > 200.0 {
        value - 273.15
    } else {
        return None;
    };
    (0.0..=125.0).contains(&celsius).then_some(celsius)
}

fn gpu_utilization(rows: &[(String, f64)]) -> Option<f64> {
    let mut engines = BTreeMap::<&str, f64>::new();
    for (name, usage) in rows {
        // Drop only the process prefix; preserve adapter/physical engine identity.
        if let Some(index) = name.find("luid_") {
            *engines.entry(&name[index..]).or_default() += usage;
        }
    }
    engines
        .values()
        .copied()
        .reduce(f64::max)
        .map(|value| value.clamp(0.0, 100.0))
}

fn cpu_frequency(core: &str) -> Option<f64> {
    let count = unsafe { GetActiveProcessorCount(0xffff) } as usize;
    if count == 0 {
        return None;
    }
    let mut values = vec![PROCESSOR_POWER_INFORMATION::default(); count];
    let status = unsafe {
        CallNtPowerInformation(
            ProcessorInformation,
            None,
            0,
            Some(values.as_mut_ptr().cast()),
            size_of_val(values.as_slice()) as u32,
        )
    };
    if status.0 < 0 {
        return None;
    }
    if let Ok(index) = core.parse::<usize>() {
        return values.get(index).map(|value| f64::from(value.CurrentMhz));
    }
    Some(
        values
            .iter()
            .map(|value| f64::from(value.CurrentMhz))
            .sum::<f64>()
            / count as f64,
    )
}

struct Library(HMODULE);
impl Library {
    fn system(name: &str) -> Result<Self> {
        let name = wide(name);
        Ok(Self(unsafe {
            LoadLibraryExW(PCWSTR(name.as_ptr()), None, LOAD_LIBRARY_SEARCH_SYSTEM32)
        }?))
    }
    unsafe fn symbol<T: Copy>(&self, name: &'static [u8]) -> Result<T> {
        let address = unsafe { GetProcAddress(self.0, PCSTR(name.as_ptr())) }
            .context("Missing optional hardware API")?;
        Ok(unsafe { std::mem::transmute_copy(&address) })
    }
}
impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            let _ = FreeLibrary(self.0);
        }
    }
}

type NvInit = unsafe extern "C" fn() -> i32;
type NvCount = unsafe extern "C" fn(*mut u32) -> i32;
type NvDevice = unsafe extern "C" fn(u32, *mut *mut c_void) -> i32;
type NvTemperature = unsafe extern "C" fn(*mut c_void, u32, *mut u32) -> i32;
struct Nvml {
    _library: Library,
    devices: Vec<*mut c_void>,
    temperature: NvTemperature,
    shutdown: NvInit,
}
impl Nvml {
    fn open() -> Result<Self> {
        let library = Library::system("nvml.dll")?;
        unsafe {
            let init: NvInit = library.symbol(b"nvmlInit_v2\0")?;
            let shutdown = library.symbol(b"nvmlShutdown\0")?;
            let count: NvCount = library.symbol(b"nvmlDeviceGetCount_v2\0")?;
            let device: NvDevice = library.symbol(b"nvmlDeviceGetHandleByIndex_v2\0")?;
            let temperature = library.symbol(b"nvmlDeviceGetTemperature\0")?;
            ensure!(init() == 0, "NVML initialization");
            let mut result = Self {
                _library: library,
                devices: Vec::new(),
                temperature,
                shutdown,
            };
            let mut n = 0;
            ensure!(count(&mut n) == 0, "NVML devices");
            for index in 0..n {
                let mut handle = std::ptr::null_mut();
                if device(index, &mut handle) == 0 {
                    result.devices.push(handle);
                }
            }
            Ok(result)
        }
    }
    fn temperature(&self) -> Option<f64> {
        self.devices
            .iter()
            .filter_map(|device| {
                let mut value = 0;
                (unsafe { (self.temperature)(*device, 0, &mut value) } == 0)
                    .then_some(f64::from(value))
            })
            .reduce(f64::max)
    }
}
impl Drop for Nvml {
    fn drop(&mut self) {
        unsafe {
            (self.shutdown)();
        }
    }
}

#[repr(C)]
struct AdlTemperature {
    size: i32,
    temperature: i32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct AdlAdapterInfo {
    size: i32,
    index: i32,
    udid: [u8; 256],
    bus: i32,
    device: i32,
    function: i32,
    vendor: i32,
    name: [u8; 256],
    display: [u8; 256],
    present: i32,
    exist: i32,
    driver_path: [u8; 256],
    driver_path_ext: [u8; 256],
    pnp: [u8; 256],
    os_display_index: i32,
}
type AdlInfo = unsafe extern "C" fn(*mut c_void, *mut AdlAdapterInfo, i32) -> i32;
type AdlCreate =
    unsafe extern "C" fn(unsafe extern "C" fn(i32) -> *mut c_void, i32, *mut *mut c_void) -> i32;
type AdlDestroy = unsafe extern "C" fn(*mut c_void) -> i32;
type AdlCount = unsafe extern "C" fn(*mut c_void, *mut i32) -> i32;
type AdlTemp = unsafe extern "C" fn(*mut c_void, i32, i32, *mut AdlTemperature) -> i32;
type AdlTempN = unsafe extern "C" fn(*mut c_void, i32, i32, *mut i32) -> i32;
unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
}
unsafe extern "C" fn adl_allocate(size: i32) -> *mut c_void {
    unsafe { malloc(size as usize) }
}
struct Adl {
    _library: Library,
    context: *mut c_void,
    adapters: Vec<i32>,
    destroy: AdlDestroy,
    old: Option<AdlTemp>,
    modern: Option<AdlTempN>,
}
impl Adl {
    fn open() -> Result<Self> {
        let library = Library::system("atiadlxx.dll")?;
        unsafe {
            let create: AdlCreate = library.symbol(b"ADL2_Main_Control_Create\0")?;
            let destroy = library.symbol(b"ADL2_Main_Control_Destroy\0")?;
            let count: AdlCount = library.symbol(b"ADL2_Adapter_NumberOfAdapters_Get\0")?;
            let info: AdlInfo = library.symbol(b"ADL2_Adapter_AdapterInfo_Get\0")?;
            let old = library.symbol(b"ADL2_Overdrive5_Temperature_Get\0").ok();
            let modern = library.symbol(b"ADL2_OverdriveN_Temperature_Get\0").ok();
            let mut context = std::ptr::null_mut();
            ensure!(
                create(adl_allocate, 1, &mut context) == 0,
                "ADL initialization"
            );
            let mut result = Self {
                _library: library,
                context,
                adapters: Vec::new(),
                destroy,
                old,
                modern,
            };
            let mut n = 0;
            ensure!(count(context, &mut n) == 0 && n >= 0, "ADL adapters");
            let mut adapters: Vec<AdlAdapterInfo> = vec![std::mem::zeroed(); n as usize];
            for adapter in &mut adapters {
                adapter.size = size_of::<AdlAdapterInfo>() as i32;
            }
            ensure!(
                info(
                    context,
                    adapters.as_mut_ptr(),
                    size_of_val(adapters.as_slice()) as i32
                ) == 0,
                "ADL adapter information"
            );
            result.adapters = adapters
                .iter()
                .filter(|adapter| adapter.exist != 0 && adapter.bus >= 0)
                .map(|adapter| adapter.index)
                .collect();
            Ok(result)
        }
    }
    fn temperature(&self) -> Option<f64> {
        self.adapters
            .iter()
            .copied()
            .filter_map(|index| unsafe {
                let mut value = 0;
                if let Some(read) = self.modern
                    && read(self.context, index, 1, &mut value) == 0
                {
                    return Some(f64::from(value) / 1000.0);
                }
                let mut value = AdlTemperature {
                    size: size_of::<AdlTemperature>() as i32,
                    temperature: 0,
                };
                self.old.and_then(|read| {
                    (read(self.context, index, 0, &mut value) == 0)
                        .then_some(f64::from(value.temperature) / 1000.0)
                })
            })
            .reduce(f64::max)
    }
}
impl Drop for Adl {
    fn drop(&mut self) {
        unsafe {
            (self.destroy)(self.context);
        }
    }
}

struct Device(HANDLE);
impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
impl Device {
    fn open(path: &str, access: u32) -> Result<Self> {
        let path = wide(path);
        Ok(Self(unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }?))
    }
    fn ioctl(&self, code: u32, input: &[u8], output: &mut [u8]) -> Result<usize> {
        let mut read = 0;
        unsafe {
            DeviceIoControl(
                self.0,
                code,
                Some(input.as_ptr().cast()),
                input.len() as u32,
                Some(output.as_mut_ptr().cast()),
                output.len() as u32,
                Some(&mut read),
                None,
            )
        }?;
        Ok(read as usize)
    }
}

struct Disk {
    path: String,
}
impl Disk {
    fn enumerate(selected: &str) -> Vec<Self> {
        let mut names = vec![0u16; 65536];
        let length = unsafe { QueryDosDeviceW(PCWSTR::null(), Some(&mut names)) } as usize;
        names[..length]
            .split(|ch| *ch == 0)
            .filter_map(|name| {
                let name = String::from_utf16_lossy(name);
                let index = name.strip_prefix("PhysicalDrive")?;
                let selected_index = selected.split_whitespace().next().unwrap_or(selected);
                (selected.is_empty()
                    || selected == "_Total"
                    || selected_index == index
                    || selected == name)
                    .then(|| Self {
                        path: format!(r"\\.\{name}"),
                    })
            })
            .collect()
    }
    fn temperature(&self) -> Option<f64> {
        // Zero-access metadata handle and explicit power-state query precede every IOCTL.
        let device = Device::open(&self.path, 0).ok()?;
        let mut awake = BOOL(0);
        if !unsafe { GetDevicePowerState(device.0, &mut awake) }.as_bool() || !awake.as_bool() {
            return None;
        }
        for property in [
            StorageDeviceTemperatureProperty,
            StorageAdapterTemperatureProperty,
        ] {
            let query = STORAGE_PROPERTY_QUERY {
                PropertyId: property,
                QueryType: PropertyStandardQuery,
                ..Default::default()
            };
            let input = unsafe {
                std::slice::from_raw_parts(
                    (&query as *const STORAGE_PROPERTY_QUERY).cast(),
                    size_of_val(&query),
                )
            };
            let mut output = vec![0u8; 4096];
            if let Ok(length) = device.ioctl(IOCTL_STORAGE_QUERY_PROPERTY, input, &mut output)
                && let Some(value) = storage_temperature(&output[..length])
            {
                return Some(value);
            }
        }
        // Legacy ATA SMART: open read/write only after proving the drive is awake.
        let device = Device::open(&self.path, 0xc0000000).ok()?;
        let input = SENDCMDINPARAMS {
            cBufferSize: 512,
            irDriveRegs: IDEREGS {
                bFeaturesReg: 0xd0,
                bSectorCountReg: 1,
                bSectorNumberReg: 1,
                bCylLowReg: 0x4f,
                bCylHighReg: 0xc2,
                bDriveHeadReg: 0xa0,
                bCommandReg: 0xb0,
                ..Default::default()
            },
            ..Default::default()
        };
        let input = unsafe {
            std::slice::from_raw_parts(
                (&input as *const SENDCMDINPARAMS).cast(),
                size_of_val(&input) - 1,
            )
        };
        let mut output = [0u8; 528];
        let length = device
            .ioctl(SMART_RCV_DRIVE_DATA, input, &mut output)
            .ok()?;
        smart_temperature(output.get(16..length)?)
    }
}

fn storage_temperature(bytes: &[u8]) -> Option<f64> {
    if bytes.len() < 24 {
        return None;
    }
    let count = u16::from_le_bytes([bytes[12], bytes[13]]) as usize;
    let records = bytes.get(24..24usize.checked_add(count.checked_mul(16)?)?)?;
    records
        .as_chunks::<16>()
        .0
        .iter()
        .filter_map(|record| {
            let value = i16::from_le_bytes([record[2], record[3]]);
            (value != i16::MIN).then_some(f64::from(value))
        })
        .reduce(f64::max)
}

fn smart_temperature(bytes: &[u8]) -> Option<f64> {
    let attributes = bytes.get(2..362)?;
    attributes
        .as_chunks::<12>()
        .0
        .iter()
        .find_map(|attribute| matches!(attribute[0], 190 | 194).then_some(f64::from(attribute[5])))
}

struct Pawn {
    device: Device,
    amd: bool,
    die_offset: f64,
}
impl Pawn {
    fn open(directory: &Path) -> std::result::Result<Self, &'static str> {
        let cpu = std::arch::x86_64::__cpuid(0);
        let amd = cpu.ebx == 0x68747541 && cpu.edx == 0x69746e65 && cpu.ecx == 0x444d4163;
        let intel = cpu.ebx == 0x756e6547 && cpu.edx == 0x49656e69 && cpu.ecx == 0x6c65746e;
        if !amd && !intel {
            return Err("monitor-cpu-temperature-unsupported");
        }
        if amd {
            let cpu = std::arch::x86_64::__cpuid(1);
            let family = ((cpu.eax >> 8) & 15) + ((cpu.eax >> 20) & 255);
            if family < 0x17 {
                return Err("monitor-cpu-temperature-unsupported");
            }
        }
        let device =
            Device::open(r"\\?\GLOBALROOT\Device\PawnIO", 0xc0000000).map_err(|error| {
                if error
                    .downcast_ref::<windows::core::Error>()
                    .is_some_and(|error| error.code() == windows::Win32::Foundation::E_ACCESSDENIED)
                {
                    "monitor-pawnio-admin-required"
                } else {
                    "monitor-pawnio-install-required"
                }
            })?;
        let binary = std::fs::read(directory.join(if amd {
            "AMDFamily17.bin"
        } else {
            "IntelMSR.bin"
        }))
        .map_err(|_| "monitor-pawnio-module-required")?;
        device
            .ioctl(0xa1b22084, &binary, &mut [])
            .map_err(|_| "monitor-pawnio-module-rejected")?;
        let mut brand = Vec::new();
        if amd {
            for leaf in 0x80000002..=0x80000004 {
                let cpu = std::arch::x86_64::__cpuid(leaf);
                for register in [cpu.eax, cpu.ebx, cpu.ecx, cpu.edx] {
                    brand.extend_from_slice(&register.to_le_bytes());
                }
            }
        }
        let brand = String::from_utf8_lossy(&brand);
        // Zen/Zen+ marketing Tctl offsets documented by AMD and the k10temp/LHM tables.
        let die_offset = if ["1600X", "1700X", "1800X"]
            .iter()
            .any(|name| brand.contains(name))
        {
            20.0
        } else if brand.contains("Threadripper 19") || brand.contains("Threadripper 29") {
            27.0
        } else if brand.contains("2700X") {
            10.0
        } else {
            0.0
        };
        Ok(Self {
            device,
            amd,
            die_offset,
        })
    }
    fn read(&self, name: &[u8], register: u64) -> Option<u64> {
        let mut input = [0u8; 40];
        input[..name.len()].copy_from_slice(name);
        input[32..].copy_from_slice(&register.to_le_bytes());
        let mut output = [0u8; 8];
        (self.device.ioctl(0xa1b22104, &input, &mut output).ok()? == 8)
            .then(|| u64::from_le_bytes(output))
    }
    fn temperature(&self) -> Option<f64> {
        if self.amd {
            let value = self.read(b"ioctl_read_smn", 0x59800)? as u32;
            let offset = if value & (1 << 19) != 0 || value & 0x30000 == 0x30000 {
                49.0
            } else {
                0.0
            };
            Some(f64::from(value >> 21) * 0.125 - offset - self.die_offset)
        } else {
            let target = ((self.read(b"ioctl_read_msr", 0x1a2)? >> 16) & 255) as f64;
            let status = self
                .read(b"ioctl_read_msr", 0x1b1)
                .filter(|value| value & (1 << 31) != 0)
                .or_else(|| {
                    self.read(b"ioctl_read_msr", 0x19c)
                        .filter(|value| value & (1 << 31) != 0)
                })?;
            (target > 0.0).then_some(target - ((status >> 16) & 127) as f64)
        }
    }
}

struct KmtAdapters(Vec<D3DKMT_ADAPTERINFO>);
impl KmtAdapters {
    fn open() -> Result<Self> {
        let mut query = D3DKMT_ENUMADAPTERS2::default();
        ensure!(
            unsafe { D3DKMTEnumAdapters2(&mut query) }.0 >= 0,
            "D3DKMTEnumAdapters2 size"
        );
        let mut adapters = vec![D3DKMT_ADAPTERINFO::default(); query.NumAdapters as usize];
        query.pAdapters = adapters.as_mut_ptr();
        ensure!(
            unsafe { D3DKMTEnumAdapters2(&mut query) }.0 >= 0,
            "D3DKMTEnumAdapters2"
        );
        adapters.truncate(query.NumAdapters as usize);
        Ok(Self(adapters))
    }
    fn temperature(&self) -> Option<f64> {
        self.0
            .iter()
            .filter_map(|adapter| {
                let mut data = D3DKMT_ADAPTER_PERFDATA::default();
                let mut query = D3DKMT_QUERYADAPTERINFO {
                    hAdapter: adapter.hAdapter,
                    Type: KMTQAITYPE_ADAPTERPERFDATA,
                    pPrivateDriverData: (&mut data as *mut D3DKMT_ADAPTER_PERFDATA).cast(),
                    PrivateDriverDataSize: size_of_val(&data) as u32,
                };
                (unsafe { D3DKMTQueryAdapterInfo(&mut query) }.0 >= 0 && data.Temperature > 0)
                    .then_some(f64::from(data.Temperature) / 10.0)
            })
            .reduce(f64::max)
    }
}
impl Drop for KmtAdapters {
    fn drop(&mut self) {
        for adapter in &self.0 {
            unsafe {
                let _ = D3DKMTCloseAdapter(&D3DKMT_CLOSEADAPTER {
                    hAdapter: adapter.hAdapter,
                });
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_temperature_reports_pawn_diagnostics_only_without_a_thermal_fallback() {
        let mut sensors = Sensors::new(
            &MonitorConfig::default(),
            SensorDemand::default(),
            Path::new("unused-sensor-module-directory"),
        );
        assert!(sensors.pawn.is_none());
        assert!(sensors.pawn_error.is_none());
        assert!(sensors.sample().unavailable.is_empty());
        sensors.demand.cpu_temperature = true;
        for error in [
            Some("monitor-cpu-temperature-unsupported"),
            Some("monitor-pawnio-admin-required"),
            Some("monitor-pawnio-install-required"),
            Some("monitor-pawnio-module-required"),
            Some("monitor-pawnio-module-rejected"),
            None,
        ] {
            sensors.pawn_error = error;
            let missing = sensors.sample();
            assert_eq!(missing.cpu_temperature_c, None);
            assert_eq!(
                missing
                    .unavailable
                    .get("cpu_temperature")
                    .map(String::as_str),
                Some(error.unwrap_or("monitor-sensor-unavailable"))
            );
            let mut thermal = SensorSnapshot {
                cpu_temperature_c: Some(47.5),
                ..Default::default()
            };
            sensors.sample_cpu_temperature(&mut thermal);
            assert_eq!(thermal.cpu_temperature_c, Some(47.5));
            assert!(thermal.unavailable.is_empty());
        }
    }

    #[test]
    fn gpu_process_instances_sum_per_engine_without_double_counting_engines() {
        let rows = vec![
            ("pid_1_luid_a_phys_0_eng_0_engtype_3D".into(), 40.0),
            ("pid_2_luid_a_phys_0_eng_0_engtype_3D".into(), 30.0),
            ("pid_1_luid_a_phys_0_eng_1_engtype_Copy".into(), 20.0),
        ];
        assert_eq!(gpu_utilization(&rows), Some(70.0));
    }
    #[test]
    fn storage_records_require_complete_wire_data() {
        let mut bytes = [0u8; 40];
        bytes[12] = 1;
        bytes[26..28].copy_from_slice(&42i16.to_le_bytes());
        assert_eq!(storage_temperature(&bytes), Some(42.0));
        assert_eq!(storage_temperature(&bytes[..39]), None);
        bytes[26..28].copy_from_slice(&i16::MIN.to_le_bytes());
        assert_eq!(storage_temperature(&bytes), None);
    }
}

#[cfg(test)]
mod thermal_tests {
    use super::thermal_temperature;

    #[test]
    fn converts_both_windows_thermal_counter_units() {
        for value in [3010.0, 301.0] {
            assert!((thermal_temperature(value).unwrap() - 27.85).abs() < 0.001);
        }
    }

    #[test]
    fn rejects_invalid_or_implausible_thermal_readings() {
        for value in [
            f64::NAN,
            f64::INFINITY,
            -1.0,
            0.0,
            200.0,
            273.0,
            399.0,
            3990.0,
        ] {
            assert_eq!(thermal_temperature(value), None);
        }
    }
}
