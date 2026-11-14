use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_STANDARD_INFO, FileStandardInfo,
        GetFileInformationByHandle, GetFileInformationByHandleEx,
    },
};

use super::{
    DIRECTORY, HARD_LINK, HIDDEN, MftNode, MftTree, MftVolume, RECORD_MASK, REPARSE_POINT, SYSTEM,
    VOLUME_PARTIAL, VOLUME_TRAVERSAL, usn_watcher,
};

const GET_NTFS_VOLUME_DATA: u32 = 0x0009_0064;
const GET_NTFS_FILE_RECORD: u32 = 0x0009_0068;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn DeviceIoControl(
        handle: *mut c_void,
        code: u32,
        input: *const c_void,
        input_len: u32,
        output: *mut c_void,
        output_len: u32,
        returned: *mut u32,
        overlapped: *mut c_void,
    ) -> i32;
    fn FindFirstVolumeW(name: *mut u16, length: u32) -> *mut c_void;
    fn FindNextVolumeW(handle: *mut c_void, name: *mut u16, length: u32) -> i32;
    fn FindVolumeClose(handle: *mut c_void) -> i32;
    fn GetVolumePathNamesForVolumeNameW(
        name: *const u16,
        paths: *mut u16,
        length: u32,
        needed: *mut u32,
    ) -> i32;
    fn GetVolumePathNameW(path: *const u16, root: *mut u16, length: u32) -> i32;
    fn GetVolumeNameForVolumeMountPointW(root: *const u16, name: *mut u16, length: u32) -> i32;
    fn GetVolumeInformationW(
        root: *const u16,
        name: *mut u16,
        name_length: u32,
        serial: *mut u32,
        max_component: *mut u32,
        flags: *mut u32,
        filesystem: *mut u16,
        filesystem_length: u32,
    ) -> i32;
    fn GetDriveTypeW(root: *const u16) -> u32;
    fn GetCompressedFileSizeW(path: *const u16, high: *mut u32) -> u32;
    fn SetLastError(error: u32);
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
fn wide_string(value: &[u16]) -> String {
    String::from_utf16_lossy(
        &value[..value
            .iter()
            .position(|item| *item == 0)
            .unwrap_or(value.len())],
    )
}
fn corrupt(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(super) fn control(
    file: &File,
    code: u32,
    input: &[u8],
    output: &mut [u8],
) -> io::Result<usize> {
    let mut returned = 0;
    let success = unsafe {
        DeviceIoControl(
            file.as_raw_handle(),
            code,
            input.as_ptr().cast(),
            input.len() as u32,
            output.as_mut_ptr().cast(),
            output.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    if returned as usize > output.len() {
        return Err(corrupt("invalid DeviceIoControl output length"));
    }
    Ok(returned as usize)
}

#[derive(Clone, Debug)]
pub(super) struct VolumeInfo {
    pub root: PathBuf,
    pub device: PathBuf,
    pub serial: u64,
    pub ntfs: bool,
    pub drive_letter: u16,
}

pub(super) struct ScanResult {
    pub tree: MftTree,
    pub warnings: Vec<String>,
}

fn describe_volume(root: PathBuf, device: PathBuf) -> io::Result<VolumeInfo> {
    let root_wide = wide(&root);
    let mut serial = 0;
    let mut filesystem = [0u16; 32];
    let success = unsafe {
        GetVolumeInformationW(
            root_wide.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    };
    if success == 0 {
        return Err(io::Error::last_os_error());
    }
    let ntfs = wide_string(&filesystem).eq_ignore_ascii_case("NTFS");
    let mut volume = VolumeInfo {
        root,
        device,
        serial: u64::from(serial),
        ntfs,
        drive_letter: 0,
    };
    let text = volume.root.to_string_lossy();
    if text.len() == 3 && text.as_bytes().get(1) == Some(&b':') {
        volume.drive_letter = text.as_bytes()[0].to_ascii_uppercase() as u16;
    }
    // NTFS exposes a 64-bit identity; GetVolumeInformation returns only its low DWORD.
    if ntfs && let Ok(file) = open_device(&volume) {
        let mut data = [0u8; 96];
        if control(&file, GET_NTFS_VOLUME_DATA, &[], &mut data).is_ok() {
            volume.serial = get_u64(&data, 0)?;
        }
    }
    Ok(volume)
}

pub(super) fn volume_for_path(path: &Path) -> Result<VolumeInfo, String> {
    let absolute = std::path::absolute(path).map_err(|error| error.to_string())?;
    let input = wide(&absolute);
    let mut root = vec![0u16; 32768];
    if unsafe { GetVolumePathNameW(input.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return Err(format!(
            "Resolve volume for {}: {}",
            path.display(),
            io::Error::last_os_error()
        ));
    }
    let mut name = [0u16; 1024];
    if unsafe {
        GetVolumeNameForVolumeMountPointW(root.as_ptr(), name.as_mut_ptr(), name.len() as u32)
    } == 0
    {
        return Err(format!(
            "Resolve local volume: {}",
            io::Error::last_os_error()
        ));
    }
    let device = wide_string(&name).trim_end_matches('\\').to_owned();
    describe_volume(PathBuf::from(wide_string(&root)), PathBuf::from(device))
        .map_err(|error| error.to_string())
}

pub(super) fn mounted_volumes() -> Result<Vec<VolumeInfo>, String> {
    let mut name = [0u16; 1024];
    let handle = unsafe { FindFirstVolumeW(name.as_mut_ptr(), name.len() as u32) };
    if handle as isize == -1 {
        return Err(io::Error::last_os_error().to_string());
    }
    struct Enumeration(*mut c_void);
    impl Drop for Enumeration {
        fn drop(&mut self) {
            unsafe {
                FindVolumeClose(self.0);
            }
        }
    }
    let enumeration = Enumeration(handle);
    let mut volumes = Vec::new();
    loop {
        let mut needed = 0;
        let mut paths = vec![0u16; 1024];
        let mut success = unsafe {
            GetVolumePathNamesForVolumeNameW(
                name.as_ptr(),
                paths.as_mut_ptr(),
                paths.len() as u32,
                &mut needed,
            )
        };
        if success == 0 && io::Error::last_os_error().raw_os_error() == Some(234) {
            paths.resize(needed as usize, 0);
            success = unsafe {
                GetVolumePathNamesForVolumeNameW(
                    name.as_ptr(),
                    paths.as_mut_ptr(),
                    paths.len() as u32,
                    &mut needed,
                )
            };
        }
        if success != 0 && paths[0] != 0 {
            // Prefer the shortest mount point, usually a drive root.
            let root = paths
                .split(|item| *item == 0)
                .filter(|part| !part.is_empty())
                .min_by_key(|part| part.len())
                .map(String::from_utf16_lossy)
                .unwrap();
            let root_wide = wide(Path::new(&root));
            let kind = unsafe { GetDriveTypeW(root_wide.as_ptr()) };
            if matches!(kind, 2 | 3 | 6) {
                let device = wide_string(&name).trim_end_matches('\\').to_owned();
                if let Ok(volume) = describe_volume(PathBuf::from(root), PathBuf::from(device)) {
                    volumes.push(volume);
                }
            }
        }
        if unsafe { FindNextVolumeW(enumeration.0, name.as_mut_ptr(), name.len() as u32) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(18) {
                return Err(error.to_string());
            }
            break;
        }
    }
    volumes.sort_by(|left, right| left.root.cmp(&right.root));
    volumes.dedup_by_key(|volume| volume.serial);
    Ok(volumes)
}

fn open_device(volume: &VolumeInfo) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(1 | 2 | 4)
        .open(&volume.device)
}

fn slice(bytes: &[u8], offset: usize, count: usize) -> io::Result<&[u8]> {
    bytes
        .get(
            offset
                ..offset
                    .checked_add(count)
                    .ok_or_else(|| corrupt("NTFS field overflow"))?,
        )
        .ok_or_else(|| corrupt("truncated NTFS field"))
}
fn get_u16(bytes: &[u8], offset: usize) -> io::Result<u16> {
    Ok(u16::from_le_bytes(
        slice(bytes, offset, 2)?.try_into().unwrap(),
    ))
}
fn get_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    Ok(u32::from_le_bytes(
        slice(bytes, offset, 4)?.try_into().unwrap(),
    ))
}
fn get_u64(bytes: &[u8], offset: usize) -> io::Result<u64> {
    Ok(u64::from_le_bytes(
        slice(bytes, offset, 8)?.try_into().unwrap(),
    ))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DataRun {
    vcn: u64,
    clusters: u64,
    lcn: Option<u64>,
}

fn runlist(bytes: &[u8], first_vcn: u64) -> io::Result<Vec<DataRun>> {
    let mut result = Vec::new();
    let mut cursor = 0;
    let mut vcn = first_vcn;
    let mut lcn = 0i64;
    loop {
        let header = *bytes
            .get(cursor)
            .ok_or_else(|| corrupt("unterminated NTFS runlist"))?;
        cursor += 1;
        if header == 0 {
            break;
        }
        let length_width = (header & 15) as usize;
        let offset_width = (header >> 4) as usize;
        if length_width == 0 || length_width > 8 || offset_width > 8 {
            return Err(corrupt("invalid NTFS run widths"));
        }
        let length_bytes = slice(bytes, cursor, length_width)?;
        cursor += length_width;
        let mut length = [0u8; 8];
        length[..length_width].copy_from_slice(length_bytes);
        let clusters = u64::from_le_bytes(length);
        if clusters == 0 {
            return Err(corrupt("zero length NTFS run"));
        }
        let physical = if offset_width == 0 {
            None
        } else {
            let offset_bytes = slice(bytes, cursor, offset_width)?;
            cursor += offset_width;
            let mut delta = [if offset_bytes[offset_width - 1] & 0x80 != 0 {
                0xff
            } else {
                0
            }; 8];
            delta[..offset_width].copy_from_slice(offset_bytes);
            lcn = lcn
                .checked_add(i64::from_le_bytes(delta))
                .ok_or_else(|| corrupt("NTFS LCN overflow"))?;
            Some(u64::try_from(lcn).map_err(|_| corrupt("negative NTFS LCN"))?)
        };
        result.push(DataRun {
            vcn,
            clusters,
            lcn: physical,
        });
        vcn = vcn
            .checked_add(clusters)
            .ok_or_else(|| corrupt("NTFS VCN overflow"))?;
    }
    Ok(result)
}

fn fixup(record: &mut [u8], sector_size: usize, already_fixed: bool) -> io::Result<()> {
    let offset = get_u16(record, 4)? as usize;
    let count = get_u16(record, 6)? as usize;
    if sector_size < 256
        || !record.len().is_multiple_of(sector_size)
        || count != record.len() / sector_size + 1
        || offset < 8
    {
        return Err(corrupt("invalid NTFS update sequence array"));
    }
    let usa = slice(record, offset, count * 2)?.to_vec();
    let marker = &usa[..2];
    let raw =
        (1..count).all(|sector| &record[sector * sector_size - 2..sector * sector_size] == marker);
    if !raw {
        if already_fixed {
            // FSCTL returns decoded cache bytes; later writes can leave USA backups stale.
            return Ok(());
        }
        return Err(corrupt("torn NTFS record (USA mismatch)"));
    }
    for sector in 1..count {
        record[sector * sector_size - 2..sector * sector_size]
            .copy_from_slice(&usa[sector * 2..sector * 2 + 2]);
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct Attribute {
    kind: u32,
    id: u16,
    name: Vec<u16>,
    lowest_vcn: u64,
    flags: u16,
    logical: u64,
    allocated: u64,
    has_compressed_size: bool,
    resident: Option<Vec<u8>>,
    runs: Vec<DataRun>,
}

#[derive(Debug)]
struct Record {
    frn: u64,
    base: u64,
    directory: bool,
    attributes: Vec<Attribute>,
}

fn parse_record(
    mut bytes: Vec<u8>,
    number: u64,
    sector: usize,
    fixed: bool,
) -> io::Result<Option<Record>> {
    if bytes.get(..4) != Some(b"FILE") {
        return Ok(None);
    }
    fixup(&mut bytes, sector, fixed)?;
    let flags = get_u16(&bytes, 22)?;
    if flags & 1 == 0 {
        return Ok(None);
    }
    let used = get_u32(&bytes, 24)? as usize;
    if used > bytes.len() || used < 48 {
        return Err(corrupt("invalid MFT record used length"));
    }
    let record = &bytes[..used];
    let frn = (number & RECORD_MASK) | (u64::from(get_u16(record, 16)?) << 48);
    let base = get_u64(record, 32)?;
    let mut cursor = get_u16(record, 20)? as usize;
    if cursor < 42 {
        return Err(corrupt("invalid MFT attribute offset"));
    }
    let mut attributes = Vec::new();
    loop {
        let kind = get_u32(record, cursor)?;
        if kind == u32::MAX {
            break;
        }
        let length = get_u32(record, cursor + 4)? as usize;
        if length < 24 || !length.is_multiple_of(8) {
            return Err(corrupt("invalid NTFS attribute length"));
        }
        let attribute = slice(record, cursor, length)?;
        let nonresident = attribute[8];
        let name_length = attribute[9] as usize;
        let name_offset = get_u16(attribute, 10)? as usize;
        let name = slice(attribute, name_offset, name_length * 2)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        let flags = get_u16(attribute, 12)?;
        let id = get_u16(attribute, 14)?;
        let mut parsed = Attribute {
            kind,
            id,
            name,
            flags,
            lowest_vcn: 0,
            logical: 0,
            allocated: 0,
            has_compressed_size: false,
            resident: None,
            runs: Vec::new(),
        };
        match nonresident {
            0 => {
                let size = get_u32(attribute, 16)? as usize;
                let offset = get_u16(attribute, 20)? as usize;
                parsed.logical = size as u64;
                parsed.resident = Some(slice(attribute, offset, size)?.to_vec());
                parsed.allocated = (length - offset) as u64;
            }
            1 => {
                if length < 64 {
                    return Err(corrupt("short nonresident NTFS attribute"));
                }
                parsed.lowest_vcn = get_u64(attribute, 16)?;
                let highest_vcn = get_u64(attribute, 24)?;
                let offset = get_u16(attribute, 32)? as usize;
                if offset < 64 {
                    return Err(corrupt("invalid NTFS mapping pairs offset"));
                }
                parsed.runs = runlist(
                    slice(attribute, offset, length - offset.min(length))?,
                    parsed.lowest_vcn,
                )?;
                if let Some(last) = parsed.runs.last()
                    && last
                        .vcn
                        .checked_add(last.clusters)
                        .and_then(|end| end.checked_sub(1))
                        != Some(highest_vcn)
                {
                    return Err(corrupt("NTFS runlist VCN range mismatch"));
                }
                parsed.logical = get_u64(attribute, 48)?;
                parsed.has_compressed_size =
                    flags & 0x8001 != 0 && parsed.lowest_vcn == 0 && offset >= 72 && length >= 72;
                parsed.allocated = if parsed.has_compressed_size {
                    get_u64(attribute, 64)?
                } else {
                    get_u64(attribute, 40)?
                };
            }
            _ => return Err(corrupt("invalid NTFS residency flag")),
        }
        attributes.push(parsed);
        cursor += length;
    }
    Ok(Some(Record {
        frn,
        base,
        directory: flags & 2 != 0,
        attributes,
    }))
}

#[derive(Clone, Debug)]
struct AttributeReference {
    kind: u32,
    name: Vec<u16>,
    lowest_vcn: u64,
    frn: u64,
    id: u16,
}

fn attribute_list(bytes: &[u8]) -> io::Result<Vec<AttributeReference>> {
    let mut references = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor..].iter().all(|byte| *byte == 0) {
            break;
        }
        let kind = get_u32(bytes, cursor)?;
        if kind == u32::MAX {
            break;
        }
        let length = get_u16(bytes, cursor + 4)? as usize;
        if length < 26 {
            return Err(corrupt("invalid attribute-list entry length"));
        }
        let entry = slice(bytes, cursor, length)?;
        let name_length = entry[6] as usize;
        let name_offset = entry[7] as usize;
        let name = slice(entry, name_offset, name_length * 2)?
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        references.push(AttributeReference {
            kind,
            name,
            lowest_vcn: get_u64(entry, 8)?,
            frn: get_u64(entry, 16)?,
            id: get_u16(entry, 24)?,
        });
        cursor += length;
    }
    Ok(references)
}

pub(super) struct RawVolume {
    pub file: File,
    sector: usize,
    cluster: u64,
    record_size: usize,
    mft_length: u64,
    mft_runs: Vec<DataRun>,
}

impl RawVolume {
    pub fn open(volume: &VolumeInfo) -> io::Result<Self> {
        let file = open_device(volume)?;
        let mut data = [0u8; 96];
        if control(&file, GET_NTFS_VOLUME_DATA, &[], &mut data)? < 96 {
            return Err(corrupt("short NTFS volume data"));
        }
        let sector = get_u32(&data, 40)? as usize;
        let cluster = u64::from(get_u32(&data, 44)?);
        let record_size = get_u32(&data, 48)? as usize;
        if !sector.is_power_of_two()
            || !(256..=65536).contains(&sector)
            || !cluster.is_power_of_two()
            || cluster < sector as u64
            || !record_size.is_power_of_two()
            || !(sector..=65536).contains(&record_size)
        {
            return Err(corrupt("invalid NTFS geometry"));
        }
        let mut raw = Self {
            file,
            sector,
            cluster,
            record_size,
            mft_length: get_u64(&data, 56)?,
            mft_runs: Vec::new(),
        };
        let offset = get_u64(&data, 64)?
            .checked_mul(cluster)
            .ok_or_else(|| corrupt("MFT offset overflow"))?;
        let mut first = vec![0u8; record_size];
        raw.read_physical(offset, &mut first)?;
        let mut record =
            parse_record(first, 0, sector, false)?.ok_or_else(|| corrupt("missing $MFT record"))?;
        for attribute in &record.attributes {
            if attribute.kind == 0x80 && attribute.name.is_empty() {
                raw.mft_runs.extend(attribute.runs.clone());
            }
        }
        raw.resolve_attributes(&mut record)?;
        raw.mft_runs = record
            .attributes
            .iter()
            .filter(|attribute| attribute.kind == 0x80 && attribute.name.is_empty())
            .flat_map(|attribute| attribute.runs.clone())
            .collect();
        raw.mft_runs.sort_by_key(|run| run.vcn);
        if raw.mft_runs.is_empty() {
            return Err(corrupt("$MFT has no data runs"));
        }
        Ok(raw)
    }

    fn read_physical(&mut self, offset: u64, output: &mut [u8]) -> io::Result<()> {
        // Volume handles require sector-aligned offsets and lengths even with buffered I/O.
        let sector = self.sector as u64;
        let aligned = offset / sector * sector;
        let prefix = (offset - aligned) as usize;
        let length = (prefix + output.len()).div_ceil(self.sector) * self.sector;
        self.file.seek(SeekFrom::Start(aligned))?;
        if prefix == 0 && length == output.len() {
            self.file.read_exact(output)
        } else {
            let mut buffer = vec![0u8; length];
            self.file.read_exact(&mut buffer)?;
            output.copy_from_slice(&buffer[prefix..prefix + output.len()]);
            Ok(())
        }
    }

    fn read_stream(&mut self, runs: &[DataRun], offset: u64, output: &mut [u8]) -> io::Result<()> {
        let mut position = offset;
        let mut written = 0;
        while written < output.len() {
            let vcn = position / self.cluster;
            let run = runs
                .iter()
                .find(|run| vcn >= run.vcn && vcn - run.vcn < run.clusters)
                .ok_or_else(|| corrupt("unmapped NTFS stream extent"))?;
            let run_start = run
                .vcn
                .checked_mul(self.cluster)
                .ok_or_else(|| corrupt("NTFS run offset overflow"))?;
            let within = position - run_start;
            let available = run
                .clusters
                .checked_mul(self.cluster)
                .and_then(|size| size.checked_sub(within))
                .ok_or_else(|| corrupt("NTFS run length overflow"))?;
            let count = available.min((output.len() - written) as u64) as usize;
            let chunk = &mut output[written..written + count];
            if let Some(lcn) = run.lcn {
                let physical = lcn
                    .checked_mul(self.cluster)
                    .and_then(|start| start.checked_add(within))
                    .ok_or_else(|| corrupt("NTFS physical offset overflow"))?;
                self.read_physical(physical, chunk)?;
            } else {
                chunk.fill(0);
            }
            written += count;
            position = position
                .checked_add(count as u64)
                .ok_or_else(|| corrupt("NTFS stream offset overflow"))?;
        }
        Ok(())
    }

    fn read_record(&mut self, number: u64) -> io::Result<Option<Record>> {
        let number = number & RECORD_MASK;
        let offset = number
            .checked_mul(self.record_size as u64)
            .ok_or_else(|| corrupt("MFT record offset overflow"))?;
        if offset >= self.mft_length {
            return Ok(None);
        }
        let mut bytes = vec![0u8; self.record_size];
        let runs = self.mft_runs.clone();
        self.read_stream(&runs, offset, &mut bytes)?;
        parse_record(bytes, number, self.sector, false)
    }

    fn read_record_control(&self, number: u64) -> io::Result<Option<Record>> {
        let mut output = vec![0u8; self.record_size + 12];
        let returned = control(
            &self.file,
            GET_NTFS_FILE_RECORD,
            &(number & RECORD_MASK).to_le_bytes(),
            &mut output,
        )?;
        if returned < 12 {
            return Err(corrupt("short FSCTL_GET_NTFS_FILE_RECORD response"));
        }
        let actual = get_u64(&output, 0)?;
        if actual & RECORD_MASK != number & RECORD_MASK {
            return Ok(None);
        }
        let length = get_u32(&output, 8)? as usize;
        let bytes = slice(&output[..returned], 12, length)?.to_vec();
        parse_record(bytes, number, self.sector, true)
    }

    fn attribute_bytes(&mut self, attribute: &Attribute) -> io::Result<Vec<u8>> {
        if let Some(bytes) = &attribute.resident {
            return Ok(bytes.clone());
        }
        if attribute.flags & 0x4001 != 0 {
            return Err(corrupt("compressed/encrypted attribute list"));
        }
        let length =
            usize::try_from(attribute.logical).map_err(|_| corrupt("attribute list too large"))?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|error| corrupt(&error.to_string()))?;
        bytes.resize(length, 0);
        self.read_stream(&attribute.runs, 0, &mut bytes)?;
        Ok(bytes)
    }

    fn resolve_attributes(&mut self, record: &mut Record) -> io::Result<()> {
        let lists: Vec<_> = record
            .attributes
            .iter()
            .filter(|attribute| attribute.kind == 0x20)
            .cloned()
            .collect();
        let mut references = VecDeque::new();
        for list in &lists {
            references.extend(attribute_list(&self.attribute_bytes(list)?)?);
        }
        let mut visited = HashSet::new();
        let mut extensions = HashMap::new();
        while let Some(reference) = references.pop_front() {
            if reference.frn == record.frn
                || !visited.insert((reference.frn, reference.id, reference.lowest_vcn))
            {
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(entry) =
                extensions.entry(reference.frn)
            {
                let extension = if record.frn & RECORD_MASK == 0 {
                    // $MFT's own extension may lie outside its bootstrapped first extent.
                    self.read_record_control(reference.frn)?
                } else {
                    match self.read_record(reference.frn) {
                        Ok(record) => record,
                        // Concurrent writes can tear extension records as well as base records.
                        Err(_) => self.read_record_control(reference.frn)?,
                    }
                };
                let extension =
                    extension.ok_or_else(|| corrupt("missing NTFS attribute-list extension"))?;
                if extension.frn != reference.frn || extension.base != record.frn {
                    return Err(corrupt("stale NTFS attribute-list reference"));
                }
                entry.insert(extension);
            }
            let extension = &extensions[&reference.frn];
            let attribute = extension
                .attributes
                .iter()
                .find(|attribute| {
                    attribute.kind == reference.kind
                        && attribute.id == reference.id
                        && attribute.name == reference.name
                        && attribute.lowest_vcn == reference.lowest_vcn
                })
                .ok_or_else(|| corrupt("missing referenced NTFS attribute"))?
                .clone();
            if attribute.kind == 0x20 {
                references.extend(attribute_list(&self.attribute_bytes(&attribute)?)?);
            }
            record.attributes.push(attribute);
        }
        Ok(())
    }

    pub fn read_entry(&mut self, number: u64) -> io::Result<Vec<Entry>> {
        // Querying the current record avoids stale MFT run mappings after a journal batch.
        let Some(mut record) = self.read_record_control(number)? else {
            return Ok(Vec::new());
        };
        if record.base != 0 {
            return Ok(Vec::new());
        }
        self.resolve_attributes(&mut record)?;
        entries(record, self.cluster)
    }

    fn scan_entries(&mut self) -> io::Result<Vec<Entry>> {
        let mut result = Vec::new();
        let records_per_chunk = (4 * 1024 * 1024 / self.record_size).max(1);
        let total = self.mft_length / self.record_size as u64;
        let runs = self.mft_runs.clone();
        let mut first = 0;
        while first < total {
            let count = (total - first).min(records_per_chunk as u64) as usize;
            let mut buffer = vec![0u8; count * self.record_size];
            self.read_stream(&runs, first * self.record_size as u64, &mut buffer)?;
            for (offset, bytes) in buffer.chunks_exact(self.record_size).enumerate() {
                let number = first + offset as u64;
                let parsed = match parse_record(bytes.to_vec(), number, self.sector, false) {
                    Ok(record) => record,
                    // A live MFT write can cross our raw read; ask NTFS for a synchronized copy.
                    Err(_) => self.read_record_control(number)?,
                };
                if let Some(mut record) = parsed {
                    if record.base != 0 {
                        continue;
                    }
                    match self.resolve_attributes(&mut record) {
                        Ok(()) => result.extend(entries(record, self.cluster)?),
                        // Attribute lists can change between reading the base and its extents.
                        Err(_) => result.extend(self.read_entry(number)?),
                    }
                }
            }
            first += count as u64;
        }
        Ok(result)
    }
}

#[derive(Clone, Debug)]
pub(super) struct Entry {
    pub frn: u64,
    pub parent: u64,
    pub name: String,
    pub logical: u64,
    pub allocated: u64,
    pub modified: i64,
    pub flags: u16,
}

fn node_flags(attributes: u32) -> u16 {
    (u16::from(attributes & FILE_ATTRIBUTE_DIRECTORY != 0) * DIRECTORY)
        | (u16::from(attributes & 2 != 0) * HIDDEN)
        | (u16::from(attributes & 4 != 0) * SYSTEM)
        | (u16::from(attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0) * REPARSE_POINT)
}

fn entries(record: Record, cluster: u64) -> io::Result<Vec<Entry>> {
    #[derive(Default)]
    struct Stream<'a> {
        head: Option<&'a Attribute>,
        mapped_clusters: u64,
        has_sparse_hole: bool,
    }

    let mut names = Vec::new();
    let mut flags = if record.directory { DIRECTORY } else { 0 };
    let mut modified = None;
    let mut logical = 0u64;
    let mut allocated = 0u64;
    let mut streams = HashMap::<&[u16], Stream<'_>>::new();
    for attribute in &record.attributes {
        match attribute.kind {
            0x10 => {
                if let Some(value) = &attribute.resident {
                    modified = Some(get_u64(value, 8)? as i64);
                    flags |= node_flags(get_u32(value, 32)?);
                }
            }
            0x30 => {
                if let Some(value) = &attribute.resident {
                    slice(value, 0, 66)?;
                    // DOS-only aliases are not extra links; Win32, POSIX and combined names are.
                    if value[65] == 2 {
                        continue;
                    }
                    let name: Vec<u16> = slice(value, 66, value[64] as usize * 2)?
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| u16::from_le_bytes(*pair))
                        .collect();
                    names.push((
                        get_u64(value, 0)?,
                        String::from_utf16_lossy(&name),
                        get_u64(value, 16)? as i64,
                    ));
                    flags |= node_flags(get_u32(value, 56)?);
                }
            }
            0x80 => {
                let stream = streams.entry(attribute.name.as_slice()).or_default();
                if attribute.lowest_vcn == 0 {
                    stream.head.get_or_insert(attribute);
                }
                for run in &attribute.runs {
                    if run.lcn.is_some() {
                        stream.mapped_clusters =
                            stream.mapped_clusters.saturating_add(run.clusters);
                    } else {
                        stream.has_sparse_hole = true;
                    }
                }
            }
            0xc0 => flags |= REPARSE_POINT,
            _ => {}
        }
    }
    for (name, stream) in streams {
        let Some(head) = stream.head else {
            continue;
        };
        let mut stream_logical = head.logical;
        let mut stream_allocated = head.allocated;
        let mapped_allocated = stream.mapped_clusters.saturating_mul(cluster);
        // $BadClus can describe the whole volume without the sparse flag; named sparse
        // streams such as $UsnJrnl:$J retain logical offsets after old data is punched out.
        if record.frn & RECORD_MASK == 8 || (!name.is_empty() && stream.has_sparse_hole) {
            stream_allocated = if head.has_compressed_size {
                stream_allocated.min(mapped_allocated)
            } else {
                mapped_allocated
            };
            stream_logical = stream_logical.min(stream_allocated);
        } else if stream.has_sparse_hole && !head.has_compressed_size {
            stream_allocated = stream_allocated.min(mapped_allocated);
        }
        logical = logical.saturating_add(stream_logical);
        allocated = allocated.saturating_add(stream_allocated);
    }
    names.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
    names.dedup_by(|left, right| left.0 == right.0 && left.1 == right.1);
    if names.len() > 1 {
        flags |= HARD_LINK;
    }
    Ok(names
        .into_iter()
        .map(|(parent, name, name_modified)| Entry {
            frn: record.frn,
            parent,
            name,
            logical,
            allocated,
            modified: modified.unwrap_or(name_modified),
            flags,
        })
        .collect())
}

pub(super) fn build_tree(
    volume: &VolumeInfo,
    mut entries: Vec<Entry>,
    journal: Option<usn_watcher::Journal>,
    flags: u16,
) -> Result<MftTree, String> {
    entries.sort_by(|left, right| {
        (left.frn, left.parent, &left.name).cmp(&(right.frn, right.parent, &right.name))
    });
    let root_position = entries
        .iter()
        .position(|entry| entry.frn == entry.parent && entry.flags & DIRECTORY != 0)
        .ok_or("volume scan did not contain its root directory")?;
    entries.swap(0, root_position);
    let root_frn = entries[0].frn;
    entries[0].name = volume.root.to_string_lossy().into_owned();
    let count = u32::try_from(entries.len()).map_err(|_| "volume exceeds 32-bit node limit")?;
    let mut directories = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.flags & DIRECTORY != 0 {
            directories.entry(entry.frn).or_insert(index as u32);
        }
    }
    let mut tree = MftTree {
        volumes: vec![MftVolume {
            volume_serial: volume.serial,
            usn_journal_id: journal.map_or(0, |journal| journal.id),
            last_usn: journal.map_or(0, |journal| journal.next),
            root_frn,
            first_node_index: 0,
            node_count: count,
            drive_letter: volume.drive_letter,
            flags,
            reserved: 0,
        }],
        nodes: Vec::with_capacity(entries.len()),
        strings: Vec::new(),
    };
    for (index, entry) in entries.into_iter().enumerate() {
        let name_offset =
            u32::try_from(tree.strings.len()).map_err(|_| "MFT string pool exceeds 4 GiB")?;
        let name_len =
            u16::try_from(entry.name.len()).map_err(|_| "MFT name exceeds 65535 UTF-8 bytes")?;
        tree.strings.extend_from_slice(entry.name.as_bytes());
        tree.nodes.push(MftNode {
            file_ref_number: entry.frn,
            logical_size: entry.logical,
            allocated_size: entry.allocated,
            modified_time_ns100: entry.modified,
            parent_index: if index == 0 {
                u32::MAX
            } else {
                directories.get(&entry.parent).copied().unwrap_or(0)
            },
            name_offset,
            name_len,
            flags: entry.flags,
            reserved: 0,
        });
    }
    tree.aggregate();
    Ok(tree)
}

pub(super) fn tree_entries(tree: &MftTree, volume: MftVolume) -> Result<Vec<Entry>, String> {
    let start = volume.first_node_index as usize;
    let end = start
        .checked_add(volume.node_count as usize)
        .ok_or("invalid volume range")?;
    let nodes = tree.nodes.get(start..end).ok_or("invalid volume range")?;
    Ok(nodes
        .iter()
        .enumerate()
        .map(|(offset, node)| Entry {
            frn: node.file_ref_number,
            parent: tree
                .nodes
                .get(node.parent_index as usize)
                .map_or(node.file_ref_number, |parent| parent.file_ref_number),
            name: tree.name(start + offset).to_owned(),
            logical: node.logical_size,
            allocated: node.allocated_size,
            modified: node.modified_time_ns100,
            flags: node.flags,
        })
        .collect())
}

pub(super) fn scan(volume: &VolumeInfo) -> Result<ScanResult, String> {
    scan_target(volume, &volume.root)
}

pub(super) fn scan_target(volume: &VolumeInfo, target: &Path) -> Result<ScanResult, String> {
    let mut fallback = volume.clone();
    let absolute = std::path::absolute(target).map_err(|error| error.to_string())?;
    fallback.root = if absolute.is_file() {
        absolute
            .parent()
            .ok_or("target file has no parent")?
            .to_path_buf()
    } else {
        absolute
    };
    if !volume.ntfs {
        return traverse(&fallback, "non-NTFS volume");
    }
    let mut journal_failure = None;
    for _ in 0..2 {
        let journal_handle = match open_device(volume) {
            Ok(file) => file,
            Err(error) if matches!(error.raw_os_error(), Some(1 | 5 | 50 | 1314)) => {
                return traverse(&fallback, &error.to_string());
            }
            Err(error) => return Err(format!("Open raw NTFS {}: {error}", volume.root.display())),
        };
        // Capture before reading even the MFT geometry: it can grow while we bootstrap.
        let before = match usn_watcher::query(&journal_handle) {
            Ok(journal) => journal,
            Err(error) if matches!(error.raw_os_error(), Some(1 | 5 | 50 | 1178 | 1179 | 1314)) => {
                return traverse(&fallback, &format!("USN journal unavailable: {error}"));
            }
            Err(error) => return Err(format!("Query USN before scan: {error}")),
        };
        let mut raw = RawVolume::open(volume)
            .map_err(|error| format!("Open raw NTFS {}: {error}", volume.root.display()))?;
        let entries = raw
            .scan_entries()
            .map_err(|error| format!("Scan raw MFT {}: {error}", volume.root.display()))?;
        // Replay from the PRE-scan cursor, never from a post-scan-only cursor.
        match usn_watcher::catch_up(volume, &mut raw, entries, before) {
            Ok(tree) => {
                return Ok(ScanResult {
                    tree,
                    warnings: journal_failure.into_iter().collect(),
                });
            }
            Err(error) => journal_failure = Some(error),
        }
    }
    Err(format!(
        "USN did not stabilize during full rescan: {}",
        journal_failure.unwrap()
    ))
}

fn file_information(path: &Path) -> io::Result<(u64, u32, u64)> {
    let file = OpenOptions::new()
        .access_mode(0)
        .share_mode(1 | 2 | 4)
        .custom_flags(0x0200_0000 | 0x0020_0000)
        .open(path)?;
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information) }
        .map_err(|error| io::Error::from_raw_os_error(error.code().0 & 0xffff))?;
    let mut standard = FILE_STANDARD_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileStandardInfo,
            (&mut standard as *mut FILE_STANDARD_INFO).cast(),
            std::mem::size_of::<FILE_STANDARD_INFO>() as u32,
        )
    }
    .map_err(|error| io::Error::from_raw_os_error(error.code().0 & 0xffff))?;
    let allocated = u64::try_from(standard.AllocationSize)
        .map_err(|_| corrupt("negative file allocation size"))?;
    Ok((
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
        information.nNumberOfLinks,
        allocated,
    ))
}

fn compressed_size(path: &Path) -> io::Result<u64> {
    let path = wide(path);
    let mut high = 0;
    unsafe {
        SetLastError(0);
    }
    let low = unsafe { GetCompressedFileSizeW(path.as_ptr(), &mut high) };
    if low == u32::MAX {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(0) {
            return Err(error);
        }
    }
    Ok((u64::from(high) << 32) | u64::from(low))
}

fn traverse(volume: &VolumeInfo, reason: &str) -> Result<ScanResult, String> {
    let root_metadata = fs::symlink_metadata(&volume.root).map_err(|error| error.to_string())?;
    let (root_frn, _, _) = file_information(&volume.root).map_err(|error| error.to_string())?;
    let root_flags = node_flags(root_metadata.file_attributes());
    if root_flags & DIRECTORY == 0 {
        return Err("directory traversal target is not a directory".into());
    }
    let descend = root_flags & REPARSE_POINT == 0;
    struct Work {
        directories: VecDeque<(PathBuf, u64)>,
        pending: usize,
        entries: Vec<Entry>,
        errors: usize,
        next_synthetic: u64,
    }
    let state = Mutex::new(Work {
        directories: if descend {
            VecDeque::from([(volume.root.clone(), root_frn)])
        } else {
            VecDeque::new()
        },
        pending: usize::from(descend),
        entries: vec![Entry {
            frn: root_frn,
            parent: root_frn,
            name: volume.root.to_string_lossy().into_owned(),
            logical: 0,
            allocated: 0,
            modified: root_metadata.last_write_time() as i64,
            flags: root_flags,
        }],
        errors: 0,
        next_synthetic: RECORD_MASK,
    });
    let changed = Condvar::new();
    let workers = std::thread::available_parallelism()
        .map_or(2, usize::from)
        .clamp(1, 8);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let (directory, parent) = {
                        let mut work = state.lock().unwrap();
                        loop {
                            if let Some(directory) = work.directories.pop_front() {
                                break directory;
                            }
                            if work.pending == 0 {
                                return;
                            }
                            work = changed.wait(work).unwrap();
                        }
                    };
                    let mut rows = Vec::new();
                    let mut descendants = Vec::new();
                    let mut errors = 0;
                    match fs::read_dir(&directory) {
                        Err(_) => errors += 1,
                        Ok(children) => {
                            for child in children {
                                let Ok(child) = child else {
                                    errors += 1;
                                    continue;
                                };
                                let path = child.path();
                                let Ok(metadata) = fs::symlink_metadata(&path) else {
                                    errors += 1;
                                    continue;
                                };
                                let attributes = metadata.file_attributes();
                                let mut flags = node_flags(attributes);
                                let (frn, links, standard_allocated) = match file_information(&path)
                                {
                                    Ok(identity) => identity,
                                    Err(_) => {
                                        errors += 1;
                                        let mut work = state.lock().unwrap();
                                        let number = work.next_synthetic;
                                        work.next_synthetic -= 1;
                                        (number, 1, 0)
                                    }
                                };
                                if links > 1 && flags & DIRECTORY == 0 {
                                    flags |= HARD_LINK;
                                }
                                let allocated = if flags & (DIRECTORY | REPARSE_POINT) != 0 {
                                    0
                                } else if attributes & (0x800 | 0x200) != 0 {
                                    match compressed_size(&path) {
                                        Ok(size) => size,
                                        Err(_) => {
                                            errors += 1;
                                            0
                                        }
                                    }
                                } else {
                                    standard_allocated
                                };
                                rows.push(Entry {
                                    frn,
                                    parent,
                                    name: child.file_name().to_string_lossy().into_owned(),
                                    logical: if flags & DIRECTORY == 0 {
                                        metadata.file_size()
                                    } else {
                                        0
                                    },
                                    allocated,
                                    modified: metadata.last_write_time() as i64,
                                    flags,
                                });
                                if flags & (DIRECTORY | REPARSE_POINT) == DIRECTORY {
                                    descendants.push((path, frn));
                                }
                            }
                        }
                    }
                    let mut work = state.lock().unwrap();
                    work.entries.extend(rows);
                    work.pending = work.pending - 1 + descendants.len();
                    work.directories.extend(descendants);
                    work.errors += errors;
                    changed.notify_all();
                }
            });
        }
    });
    let work = state.into_inner().unwrap();
    let flags = VOLUME_TRAVERSAL | if work.errors > 0 { VOLUME_PARTIAL } else { 0 };
    let mut warnings = vec![format!(
        "{}: OnDemandOnly directory traversal ({reason})",
        volume.root.display()
    )];
    if work.errors > 0 {
        warnings.push(format!(
            "{}: {} inaccessible entries/directories or allocation sizes; results are partial",
            volume.root.display(),
            work.errors
        ));
    }
    Ok(ScanResult {
        tree: build_tree(volume, work.entries, None, flags)?,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runlists_decode_signed_deltas_and_sparse_extents() {
        let runs = runlist(&[0x11, 3, 10, 0x01, 2, 0x11, 1, 0xfd, 0], 4).unwrap();
        assert_eq!(
            runs,
            vec![
                DataRun {
                    vcn: 4,
                    clusters: 3,
                    lcn: Some(10)
                },
                DataRun {
                    vcn: 7,
                    clusters: 2,
                    lcn: None
                },
                DataRun {
                    vcn: 9,
                    clusters: 1,
                    lcn: Some(7)
                }
            ]
        );
        assert!(runlist(&[0x11, 1, 0xff, 0], 0).is_err());
        assert!(runlist(&[0x11, 0, 1, 0], 0).is_err());
        assert!(runlist(&[0x11, 1], 0).is_err());
    }

    #[test]
    fn usa_restores_sector_tails_and_rejects_torn_records() {
        let mut bytes = vec![0u8; 1024];
        bytes[4..6].copy_from_slice(&48u16.to_le_bytes());
        bytes[6..8].copy_from_slice(&3u16.to_le_bytes());
        bytes[48..54].copy_from_slice(&[0xaa, 0xbb, 1, 2, 3, 4]);
        bytes[510..512].copy_from_slice(&[0xaa, 0xbb]);
        bytes[1022..1024].copy_from_slice(&[0xaa, 0xbb]);
        let mut torn = bytes.clone();
        torn[510] = 0;
        assert!(fixup(&mut torn, 512, false).is_err());
        fixup(&mut bytes, 512, false).unwrap();
        assert_eq!(&bytes[510..512], &[1, 2]);
        assert_eq!(&bytes[1022..1024], &[3, 4]);
        assert!(fixup(&mut bytes, 512, false).is_err());
        fixup(&mut bytes, 512, true).unwrap();

        bytes[510..512].copy_from_slice(&[9, 10]);
        bytes[1022..1024].copy_from_slice(&[11, 12]);
        assert!(fixup(&mut bytes, 512, false).is_err());
        let cached = bytes.clone();
        fixup(&mut bytes, 512, true).unwrap();
        assert_eq!(bytes, cached);

        bytes[6..8].copy_from_slice(&2u16.to_le_bytes());
        assert!(fixup(&mut bytes, 512, true).is_err());
    }

    fn filename(parent: u64, text: &str, namespace: u8) -> Attribute {
        let name: Vec<u16> = text.encode_utf16().collect();
        let mut value = vec![0u8; 66 + name.len() * 2];
        value[..8].copy_from_slice(&parent.to_le_bytes());
        value[64] = name.len() as u8;
        value[65] = namespace;
        for (target, code) in value[66..].as_chunks_mut::<2>().0.iter_mut().zip(name) {
            target.copy_from_slice(&code.to_le_bytes());
        }
        Attribute {
            kind: 0x30,
            id: 1,
            name: Vec::new(),
            lowest_vcn: 0,
            flags: 0,
            logical: 0,
            allocated: 0,
            has_compressed_size: false,
            resident: Some(value),
            runs: Vec::new(),
        }
    }

    fn data_stream(name: &str, logical: u64, allocated: u64, runs: Vec<DataRun>) -> Attribute {
        Attribute {
            kind: 0x80,
            id: 2,
            name: name.encode_utf16().collect(),
            lowest_vcn: runs.first().map_or(0, |run| run.vcn),
            flags: 0,
            logical,
            allocated,
            has_compressed_size: false,
            resident: None,
            runs,
        }
    }

    #[test]
    fn healthy_badclus_does_not_count_the_volume_sized_sparse_stream() {
        let volume_size = 450 * 1024 * 1024 * 1024;
        let record = Record {
            frn: (0x42 << 48) | 8,
            base: 0,
            directory: false,
            attributes: vec![
                filename(5, "$BadClus", 1),
                data_stream(
                    "$Bad",
                    volume_size,
                    volume_size,
                    vec![DataRun {
                        vcn: 0,
                        clusters: 117_964_800,
                        lcn: None,
                    }],
                ),
            ],
        };
        let rows = entries(record, 4096).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].logical, rows[0].allocated), (0, 0));
    }

    #[test]
    fn badclus_counts_physical_bad_clusters_without_requiring_sparse_flags_or_holes() {
        for name in ["", "$Bad"] {
            let record = Record {
                frn: 8,
                base: 0,
                directory: false,
                attributes: vec![
                    filename(5, "$BadClus", 1),
                    data_stream(
                        name,
                        450 * 1024 * 1024 * 1024,
                        450 * 1024 * 1024 * 1024,
                        vec![DataRun {
                            vcn: 0,
                            clusters: 2,
                            lcn: Some(100),
                        }],
                    ),
                ],
            };
            let rows = entries(record, 8192).unwrap();
            assert_eq!((rows[0].logical, rows[0].allocated), (16384, 16384));
        }
    }

    #[test]
    fn named_sparse_streams_combine_all_extents_before_clamping_sizes() {
        for (has_compressed_size, allocated, expected) in [
            (false, 4096, 12288),
            (true, 4096, 4096),
            (true, 16384, 12288),
        ] {
            let mut head = data_stream(
                "$J",
                450 * 1024 * 1024 * 1024,
                allocated,
                vec![DataRun {
                    vcn: 0,
                    clusters: 1,
                    lcn: Some(10),
                }],
            );
            head.has_compressed_size = has_compressed_size;
            let extension = data_stream(
                "$J",
                u64::MAX,
                u64::MAX,
                vec![
                    DataRun {
                        vcn: 1,
                        clusters: 7,
                        lcn: None,
                    },
                    DataRun {
                        vcn: 8,
                        clusters: 2,
                        lcn: Some(20),
                    },
                ],
            );
            let other = data_stream(
                "",
                5000,
                8192,
                vec![DataRun {
                    vcn: 0,
                    clusters: 2,
                    lcn: Some(30),
                }],
            );
            let record = Record {
                frn: 42,
                base: 0,
                directory: false,
                attributes: vec![filename(5, "$UsnJrnl", 1), extension, other, head],
            };
            let rows = entries(record, 4096).unwrap();
            assert_eq!(rows[0].logical, 5000 + expected);
            assert_eq!(rows[0].allocated, 8192 + expected);
        }
    }

    #[test]
    fn unnamed_sparse_streams_keep_logical_size_and_bound_physical_allocation() {
        for (has_compressed_size, allocated, expected) in [
            (false, 32768, 8192),
            (false, 4096, 4096),
            (true, 4096, 4096),
        ] {
            let mut data = data_stream(
                "",
                30000,
                allocated,
                vec![
                    DataRun {
                        vcn: 0,
                        clusters: 2,
                        lcn: Some(10),
                    },
                    DataRun {
                        vcn: 2,
                        clusters: 6,
                        lcn: None,
                    },
                ],
            );
            data.has_compressed_size = has_compressed_size;
            let record = Record {
                frn: 99,
                base: 0,
                directory: false,
                attributes: vec![filename(5, "sparse", 1), data],
            };
            let rows = entries(record, 4096).unwrap();
            assert_eq!(rows[0].logical, 30000);
            assert_eq!(rows[0].allocated, expected);
        }
    }

    #[test]
    fn all_hardlink_names_survive_but_dos_aliases_do_not() {
        let record = Record {
            frn: 123,
            base: 0,
            directory: false,
            attributes: vec![
                filename(5, "long name", 1),
                filename(5, "LONGNA~1", 2),
                filename(9, "other", 0),
            ],
        };
        let rows = entries(record, 4096).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].parent, 5);
        assert_eq!(rows[1].parent, 9);
        assert!(rows.iter().all(|row| row.flags & HARD_LINK != 0));
    }

    #[test]
    fn attribute_list_preserves_sequence_and_extent_identity() {
        let mut bytes = vec![0u8; 32];
        bytes[..4].copy_from_slice(&0x80u32.to_le_bytes());
        bytes[4..6].copy_from_slice(&32u16.to_le_bytes());
        bytes[8..16].copy_from_slice(&99u64.to_le_bytes());
        bytes[16..24].copy_from_slice(&0xabcd_0000_0000_1234u64.to_le_bytes());
        bytes[24..26].copy_from_slice(&7u16.to_le_bytes());
        let references = attribute_list(&bytes).unwrap();
        assert_eq!(references[0].lowest_vcn, 99);
        assert_eq!(references[0].frn, 0xabcd_0000_0000_1234);
        assert_eq!(references[0].id, 7);
        bytes[4..6].copy_from_slice(&8u16.to_le_bytes());
        assert!(attribute_list(&bytes).is_err());
    }

    #[test]
    fn raw_record_preserves_sequence_time_and_sparse_physical_allocation() {
        fn resident(kind: u32, value: &[u8]) -> Vec<u8> {
            let length = (24 + value.len()).div_ceil(8) * 8;
            let mut attribute = vec![0u8; length];
            attribute[..4].copy_from_slice(&kind.to_le_bytes());
            attribute[4..8].copy_from_slice(&(length as u32).to_le_bytes());
            attribute[16..20].copy_from_slice(&(value.len() as u32).to_le_bytes());
            attribute[20..22].copy_from_slice(&24u16.to_le_bytes());
            attribute[24..24 + value.len()].copy_from_slice(value);
            attribute
        }
        let mut standard = [0u8; 48];
        standard[8..16].copy_from_slice(&(-123i64).to_le_bytes());
        standard[32..36].copy_from_slice(&2u32.to_le_bytes());
        let name = filename(5, "sparse", 1).resident.unwrap();
        let mut data = vec![0u8; 80];
        data[..4].copy_from_slice(&0x80u32.to_le_bytes());
        data[4..8].copy_from_slice(&80u32.to_le_bytes());
        data[8] = 1;
        data[12..14].copy_from_slice(&0x8000u16.to_le_bytes());
        data[24..32].copy_from_slice(&7u64.to_le_bytes());
        data[32..34].copy_from_slice(&72u16.to_le_bytes());
        data[40..48].copy_from_slice(&32768u64.to_le_bytes());
        data[48..56].copy_from_slice(&30000u64.to_le_bytes());
        data[64..72].copy_from_slice(&4096u64.to_le_bytes());
        data[72..78].copy_from_slice(&[0x11, 1, 8, 0x01, 7, 0]);
        let mut bytes = vec![0u8; 1024];
        bytes[..4].copy_from_slice(b"FILE");
        bytes[4..6].copy_from_slice(&48u16.to_le_bytes());
        bytes[6..8].copy_from_slice(&3u16.to_le_bytes());
        bytes[16..18].copy_from_slice(&0x42u16.to_le_bytes());
        bytes[20..22].copy_from_slice(&56u16.to_le_bytes());
        bytes[22..24].copy_from_slice(&1u16.to_le_bytes());
        bytes[48..54].copy_from_slice(&[0xaa, 0xbb, 0, 0, 0, 0]);
        let mut cursor = 56;
        for attribute in [resident(0x10, &standard), resident(0x30, &name), data] {
            bytes[cursor..cursor + attribute.len()].copy_from_slice(&attribute);
            cursor += attribute.len();
        }
        bytes[cursor..cursor + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[24..28].copy_from_slice(&((cursor + 4) as u32).to_le_bytes());
        bytes[510..512].copy_from_slice(&[0xaa, 0xbb]);
        bytes[1022..1024].copy_from_slice(&[0xaa, 0xbb]);
        let record = parse_record(bytes.clone(), 99, 512, false)
            .unwrap()
            .unwrap();
        let rows = entries(record, 4096).unwrap();
        assert_eq!(rows[0].frn, (0x42 << 48) | 99);
        assert_eq!(rows[0].logical, 30000);
        assert_eq!(rows[0].allocated, 4096);
        assert_eq!(rows[0].modified, -123);
        assert_eq!(rows[0].flags & HIDDEN, HIDDEN);

        let data_offset = 56 + resident(0x10, &standard).len() + resident(0x30, &name).len();
        for flags in [0u16, 1, 0x8000, 0x8001] {
            let mut without_compressed_size = bytes.clone();
            let data = &mut without_compressed_size[data_offset..data_offset + 72];
            data[4..8].copy_from_slice(&72u32.to_le_bytes());
            data[12..14].copy_from_slice(&flags.to_le_bytes());
            data[32..34].copy_from_slice(&64u16.to_le_bytes());
            data[64..72].copy_from_slice(&[0x11, 1, 8, 0x01, 7, 0, 0, 0]);
            let end = data_offset + 72;
            without_compressed_size[end..end + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            without_compressed_size[24..28].copy_from_slice(&((end + 4) as u32).to_le_bytes());
            let record = parse_record(without_compressed_size, 99, 512, false)
                .unwrap()
                .unwrap();
            assert_eq!(record.attributes[2].allocated, 32768);
            let rows = entries(record, 4096).unwrap();
            assert_eq!((rows[0].logical, rows[0].allocated), (30000, 4096));
        }

        let resident_data = resident(0x80, &[0; 41]);
        bytes[data_offset..data_offset + resident_data.len()].copy_from_slice(&resident_data);
        let end = data_offset + resident_data.len();
        bytes[end..end + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        bytes[24..28].copy_from_slice(&((end + 4) as u32).to_le_bytes());
        let record = parse_record(bytes, 99, 512, false).unwrap().unwrap();
        let rows = entries(record, 4096).unwrap();
        assert_eq!(rows[0].logical, 41);
        assert_eq!(rows[0].allocated, 48);
    }
}
