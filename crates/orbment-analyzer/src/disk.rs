use crate::{
    file_search::{Query, normalized_path},
    mft_engine::MftTree,
    treemap::Node,
};
use orbment_core::{config::DiskAnalyzerConfig, i18n::tr};
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashSet},
    fs::{File, OpenOptions},
    io::Read,
    path::Path,
};

const DIRECTORY: u16 = 1;
const REPARSE_POINT: u16 = 16;
const RECORD_MASK: u64 = 0x0000_ffff_ffff_ffff;

#[derive(Clone, Debug, Serialize)]
pub struct DiskEntry {
    pub path: String,
    pub name: String,
    pub logical_size: u64,
    pub allocated_size: u64,
    pub is_directory: bool,
    pub file_ref_number: u64,
    pub volume_serial: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExtensionStat {
    pub extension: String,
    pub file_count: u64,
    pub logical_size: u64,
    pub allocated_size: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct DiskReport {
    pub target: String,
    pub treemap: Node,
    pub top_files: Vec<DiskEntry>,
    pub extensions: Vec<ExtensionStat>,
    pub logical_size: u64,
    pub allocated_size: u64,
    pub file_count: u64,
    pub directory_count: u64,
}

fn identity(volume_serial: u64, file_ref_number: u64) -> (u64, u64) {
    (volume_serial, file_ref_number & RECORD_MASK)
}

struct Subtree {
    entries: Vec<DiskEntry>,
    sources: Vec<usize>,
    parents: Vec<Option<usize>>,
}

// Parents precede descendants. A physical file belongs to its first encountered path
// within this target, so totals add up without counting hard-link aliases twice.
fn collect(tree: &MftTree, target: &Path) -> Result<Subtree, String> {
    let target_text = target.to_string_lossy();
    let normalized = normalized_path(&target_text);
    if normalized.is_empty() {
        return Err("A disk analysis target is required".into());
    }
    let basename = normalized.rsplit('\\').next().unwrap_or(&normalized);
    let root = tree
        .nodes
        .iter()
        .enumerate()
        .find_map(|(index, node)| {
            if node.parent_index != u32::MAX
                && !tree.name(index).eq_ignore_ascii_case(basename)
                && tree.name(index).to_lowercase() != basename
            {
                return None;
            }
            (normalized_path(&tree.path(index).to_string_lossy()) == normalized).then_some(index)
        })
        .ok_or_else(|| format!("Target is not present in the MFT tree: {target_text}"))?;
    let mut first_child = vec![usize::MAX; tree.nodes.len()];
    let mut next_sibling = vec![usize::MAX; tree.nodes.len()];
    // Reverse insertion preserves the snapshot's deterministic sibling order.
    for (index, node) in tree.nodes.iter().enumerate().rev() {
        let parent = node.parent_index as usize;
        if parent < tree.nodes.len() {
            next_sibling[index] = first_child[parent];
            first_child[parent] = index;
        }
    }
    let mut volumes = vec![0u64; tree.nodes.len()];
    for volume in &tree.volumes {
        let start = volume.first_node_index as usize;
        let end = start + volume.node_count as usize;
        let range = volumes
            .get_mut(start..end)
            .ok_or("MFT volume node range is invalid")?;
        range.fill(volume.volume_serial);
    }
    let mut entries = Vec::new();
    let mut sources = Vec::new();
    let mut parents = Vec::new();
    let mut seen = HashSet::new();
    let mut pending = vec![(root, None)];
    while let Some((index, parent)) = pending.pop() {
        let node = &tree.nodes[index];
        if !seen.insert(identity(volumes[index], node.file_ref_number)) {
            continue;
        }
        let position = entries.len();
        let is_directory = node.flags & DIRECTORY != 0;
        entries.push(DiskEntry {
            path: tree.path(index).to_string_lossy().into_owned(),
            name: tree.name(index).to_owned(),
            logical_size: if is_directory { 0 } else { node.logical_size },
            allocated_size: if is_directory { 0 } else { node.allocated_size },
            is_directory,
            file_ref_number: node.file_ref_number,
            volume_serial: volumes[index],
        });
        sources.push(index);
        parents.push(parent);
        if is_directory && node.flags & REPARSE_POINT == 0 {
            let mut children = Vec::new();
            let mut child = first_child[index];
            while child != usize::MAX {
                children.push(child);
                child = next_sibling[child];
            }
            pending.extend(
                children
                    .into_iter()
                    .rev()
                    .map(|child| (child, Some(position))),
            );
        }
    }
    for index in (1..entries.len()).rev() {
        if let Some(parent) = parents[index] {
            let logical = entries[index].logical_size;
            let allocated = entries[index].allocated_size;
            entries[parent].logical_size = entries[parent]
                .logical_size
                .checked_add(logical)
                .ok_or("Logical disk size overflow")?;
            entries[parent].allocated_size = entries[parent]
                .allocated_size
                .checked_add(allocated)
                .ok_or("Allocated disk size overflow")?;
        }
    }
    Ok(Subtree {
        entries,
        sources,
        parents,
    })
}

fn sort_entries(entries: &mut Vec<DiskEntry>, limit: usize, allocated: bool) {
    let order = |a: &DiskEntry, b: &DiskEntry| {
        let a_size = if allocated {
            a.allocated_size
        } else {
            a.logical_size
        };
        let b_size = if allocated {
            b.allocated_size
        } else {
            b.logical_size
        };
        b_size.cmp(&a_size).then_with(|| a.path.cmp(&b.path))
    };
    if entries.len() > limit {
        entries.select_nth_unstable_by(limit, order);
        entries.truncate(limit);
    }
    entries.sort_unstable_by(order);
}

pub fn analyze(
    tree: &MftTree,
    target: &Path,
    config: &DiskAnalyzerConfig,
    filter: &str,
) -> Result<DiskReport, String> {
    let query = Query::parse(filter)?;
    let subtree = collect(tree, target)?;
    let root = &subtree.entries[0];
    let mut extensions = BTreeMap::<String, ExtensionStat>::new();
    let mut top_files = Vec::new();
    let mut file_count = 0u64;
    let mut directory_count = 0u64;
    let mut visible = vec![false; subtree.entries.len()];
    let mut inherited_match = vec![false; subtree.entries.len()];
    for (position, entry) in subtree.entries.iter().enumerate() {
        let source = &tree.nodes[subtree.sources[position]];
        let matches = query
            .score(
                &entry.name,
                &entry.path,
                entry.logical_size,
                source.modified_time_ns100,
                entry.is_directory,
            )
            .is_some();
        let parent_matches =
            subtree.parents[position].is_some_and(|parent| inherited_match[parent]);
        inherited_match[position] = parent_matches || (entry.is_directory && matches);
        visible[position] = matches || parent_matches;
        if entry.is_directory {
            directory_count += 1;
        } else {
            file_count += 1;
            let extension = Path::new(&entry.name)
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let stat = extensions
                .entry(extension.clone())
                .or_insert_with(|| ExtensionStat {
                    extension,
                    file_count: 0,
                    logical_size: 0,
                    allocated_size: 0,
                });
            stat.file_count += 1;
            stat.logical_size += entry.logical_size;
            stat.allocated_size += entry.allocated_size;
            if visible[position] {
                top_files.push(entry.clone());
            }
        }
    }
    // Filtering changes the visible tree and file list, never the target's accounting.
    let mut nodes: Vec<Option<Node>> = subtree
        .entries
        .iter()
        .map(|entry| {
            Some(Node {
                id: entry.path.clone(),
                label: entry.name.clone(),
                bytes: 0,
                children: Vec::new(),
            })
        })
        .collect();
    for position in (0..subtree.entries.len()).rev() {
        let entry = &subtree.entries[position];
        let mut node = nodes[position]
            .take()
            .expect("Each disk tree node is consumed once");
        if !entry.is_directory && visible[position] {
            node.bytes = if config.show_allocated_size {
                entry.allocated_size
            } else {
                entry.logical_size
            };
        }
        node.children
            .sort_unstable_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.id.cmp(&b.id)));
        if let Some(parent) = subtree.parents[position] {
            if visible[position] || !node.children.is_empty() {
                let parent = nodes[parent]
                    .as_mut()
                    .expect("Disk traversal visits descendants before parents");
                parent.bytes += node.bytes;
                parent.children.push(node);
            }
        } else {
            nodes[position] = Some(node);
        }
    }
    let mut treemap = nodes[0]
        .take()
        .expect("Disk target always produces a root node");
    if config.show_free_space
        && tree.nodes[subtree.sources[0]].parent_index == u32::MAX
        && let Some(free) = volume_free_space(Path::new(&root.path))?
    {
        treemap.children.push(Node {
            id: format!("orbment:free-space:{}", root.volume_serial),
            label: tr("file_tools.free_space"),
            bytes: free,
            children: Vec::new(),
        });
        treemap.bytes = treemap
            .bytes
            .checked_add(free)
            .ok_or("Disk treemap size overflow")?;
    }
    sort_entries(
        &mut top_files,
        config.max_top_files as usize,
        config.show_allocated_size,
    );
    let mut extensions: Vec<_> = extensions.into_values().collect();
    extensions.sort_unstable_by(|a, b| {
        let a_size = if config.show_allocated_size {
            a.allocated_size
        } else {
            a.logical_size
        };
        let b_size = if config.show_allocated_size {
            b.allocated_size
        } else {
            b.logical_size
        };
        b_size
            .cmp(&a_size)
            .then_with(|| a.extension.cmp(&b.extension))
    });
    Ok(DiskReport {
        target: root.path.clone(),
        treemap,
        top_files,
        extensions,
        logical_size: root.logical_size,
        allocated_size: root.allocated_size,
        file_count,
        directory_count,
    })
}

pub fn top_consumers(
    tree: &MftTree,
    target: &Path,
    limit: usize,
    allocated: bool,
) -> Result<Vec<DiskEntry>, String> {
    let mut entries = collect(tree, target)?.entries;
    // The selected directory is the container, not one of its own consumers.
    if entries.first().is_some_and(|entry| entry.is_directory) {
        entries.remove(0);
    }
    sort_entries(&mut entries, limit, allocated);
    Ok(entries)
}

pub fn files_in_subtree(tree: &MftTree, target: &Path) -> Result<Vec<DiskEntry>, String> {
    Ok(collect(tree, target)?
        .entries
        .into_iter()
        .filter(|entry| !entry.is_directory)
        .collect())
}

fn volume_free_space(path: &Path) -> Result<Option<u64>, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetVolumePathNameW},
        core::PCWSTR,
    };
    let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut mount = vec![0u16; 32768];
    unsafe { GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut mount) }
        .map_err(|e| format!("Resolve volume root for {}: {e}", path.display()))?;
    let length = mount
        .iter()
        .position(|&ch| ch == 0)
        .ok_or("Volume root path is not terminated")?;
    let root = String::from_utf16_lossy(&mount[..length]);
    // A targeted fallback scan also has a parentless node, but it is not a volume.
    if normalized_path(&path.to_string_lossy()) != normalized_path(&root) {
        return Ok(None);
    }
    let mut free = 0;
    unsafe { GetDiskFreeSpaceExW(PCWSTR(mount.as_ptr()), None, None, Some(&mut free)) }
        .map_err(|e| format!("Read free space for {}: {e}", path.display()))?;
    Ok(Some(free))
}

fn duplicate_candidates(entries: &[DiskEntry]) -> BTreeMap<u64, Vec<&DiskEntry>> {
    let mut seen = HashSet::new();
    let mut paths = HashSet::new();
    let mut sizes = BTreeMap::<u64, Vec<&DiskEntry>>::new();
    for entry in entries {
        if !entry.is_directory
            && seen.insert(identity(entry.volume_serial, entry.file_ref_number))
            && paths.insert(normalized_path(&entry.path))
        {
            sizes.entry(entry.logical_size).or_default().push(entry);
        }
    }
    sizes.retain(|_, entries| entries.len() > 1);
    sizes
}

/// Only an explicit user request calls this. Files are grouped by logical size first;
/// no hashes, persistent content cache, or background file reads are involved.
pub fn find_duplicates(entries: &[DiskEntry]) -> Result<Vec<Vec<String>>, String> {
    let mut duplicates = Vec::new();
    for candidates in duplicate_candidates(entries).into_values() {
        let mut groups: Vec<Vec<&DiskEntry>> = Vec::new();
        for candidate in candidates {
            let mut matching = None;
            for (index, group) in groups.iter().enumerate() {
                if same_contents(group[0], candidate)? {
                    matching = Some(index);
                    break;
                }
            }
            if let Some(index) = matching {
                groups[index].push(candidate);
            } else {
                groups.push(vec![candidate]);
            }
        }
        duplicates.extend(
            groups
                .into_iter()
                .filter(|group| group.len() > 1)
                .map(|group| {
                    group
                        .into_iter()
                        .map(|entry| entry.path.clone())
                        .collect::<Vec<_>>()
                }),
        );
    }
    for group in &mut duplicates {
        group.sort();
    }
    duplicates.sort();
    Ok(duplicates)
}

fn open_comparison(entry: &DiskEntry) -> Result<File, String> {
    use std::os::windows::fs::OpenOptionsExt;
    // Denying write sharing gives each comparison a stable view and fails explicitly
    // for files currently open for writing instead of reporting a racy duplicate.
    let file = OpenOptions::new()
        .read(true)
        .share_mode(1 | 4)
        .open(&entry.path)
        .map_err(|e| format!("Open duplicate candidate {}: {e}", entry.path))?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("Inspect duplicate candidate {}: {e}", entry.path))?;
    if !metadata.is_file() || metadata.len() != entry.logical_size {
        return Err(format!(
            "Duplicate candidate changed since indexing: {}",
            entry.path
        ));
    }
    Ok(file)
}

fn live_identity(file: &File, path: &str) -> Result<(u64, [u8; 16]), String> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{FILE_ID_INFO, FileIdInfo, GetFileInformationByHandleEx},
    };
    let mut info = FILE_ID_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    }
    .map_err(|e| format!("Read duplicate candidate identity {path}: {e}"))?;
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

fn same_contents(left: &DiskEntry, right: &DiskEntry) -> Result<bool, String> {
    if left.logical_size != right.logical_size
        || identity(left.volume_serial, left.file_ref_number)
            == identity(right.volume_serial, right.file_ref_number)
    {
        return Ok(false);
    }
    let mut left_file = open_comparison(left)?;
    let mut right_file = open_comparison(right)?;
    // Recheck opened handles: a path may have become a hard link since the scan.
    if live_identity(&left_file, &left.path)? == live_identity(&right_file, &right.path)? {
        return Ok(false);
    }
    let mut left_bytes = vec![0u8; 64 * 1024];
    let mut right_bytes = vec![0u8; 64 * 1024];
    let mut remaining = left.logical_size;
    while remaining > 0 {
        let length = remaining.min(left_bytes.len() as u64) as usize;
        left_file
            .read_exact(&mut left_bytes[..length])
            .map_err(|e| format!("Read duplicate candidate {}: {e}", left.path))?;
        right_file
            .read_exact(&mut right_bytes[..length])
            .map_err(|e| format!("Read duplicate candidate {}: {e}", right.path))?;
        if left_bytes[..length] != right_bytes[..length] {
            return Ok(false);
        }
        remaining -= length as u64;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mft_engine::{MftNode, MftVolume};

    fn fixture() -> MftTree {
        let mut tree = MftTree {
            volumes: Vec::new(),
            nodes: Vec::new(),
            strings: Vec::new(),
        };
        for (name, parent, frn, flags, logical, allocated) in [
            ("C:\\", u32::MAX, 5, DIRECTORY, 0, 0),
            ("docs", 0, 6, DIRECTORY, 0, 0),
            ("one.txt", 1, 0x0001_0000_0000_0020, 8, 100, 4096),
            ("alias.txt", 0, 0x0002_0000_0000_0020, 8, 100, 4096),
            ("video.mp4", 0, 33, 0, 200, 8192),
            ("junction", 0, 34, DIRECTORY | REPARSE_POINT, 0, 0),
            ("outside.bin", 5, 35, 0, 1000, 4096),
        ] {
            let offset = tree.strings.len() as u32;
            tree.strings.extend_from_slice(name.as_bytes());
            tree.nodes.push(MftNode {
                file_ref_number: frn,
                logical_size: logical,
                allocated_size: allocated,
                modified_time_ns100: 0,
                parent_index: parent,
                name_offset: offset,
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
            node_count: tree.nodes.len() as u32,
            drive_letter: b'C' as u16,
            flags: 0,
            reserved: 0,
        });
        tree
    }

    #[test]
    fn subtree_deduplicates_low48_frns_and_never_follows_junctions() {
        let tree = fixture();
        let config = DiskAnalyzerConfig::default();
        let report = analyze(&tree, Path::new("C:\\"), &config, "").unwrap();
        assert_eq!(report.logical_size, 300);
        assert_eq!(report.allocated_size, 12288);
        assert_eq!(report.file_count, 2);
        assert_eq!(report.directory_count, 3);
        assert_eq!(report.treemap.bytes, 12288);
        let subtree = analyze(&tree, Path::new("C:\\docs"), &config, "").unwrap();
        assert_eq!(subtree.logical_size, 100);
        let filtered = analyze(&tree, Path::new("C:\\"), &config, "*.mp4").unwrap();
        assert_eq!(filtered.logical_size, 300);
        assert_eq!(filtered.treemap.bytes, 8192);
        assert_eq!(filtered.top_files.len(), 1);
    }

    #[test]
    fn duplicate_candidates_exclude_hardlinks_but_keep_equal_frns_on_other_volumes() {
        let entry = |path: &str, volume_serial, file_ref_number, logical_size| DiskEntry {
            path: path.into(),
            name: path.into(),
            logical_size,
            allocated_size: 4096,
            is_directory: false,
            volume_serial,
            file_ref_number,
        };
        let entries = vec![
            entry("C:\\one", 1, 0x0001_0000_0000_0020, 100),
            entry("C:\\alias", 1, 0x0002_0000_0000_0020, 100),
            entry("D:\\other", 2, 0x0001_0000_0000_0020, 100),
            entry("C:\\large", 1, 33, 200),
        ];
        let groups = duplicate_candidates(&entries);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[&100].len(), 2);
        assert_eq!(groups[&100][1].path, "D:\\other");
    }
}
