mod ntfs_scanner;
pub mod snapshot;
mod usn_watcher;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use orbment_core::config::FileSearchConfig;

pub const DIRECTORY: u16 = 1;
pub const HIDDEN: u16 = 2;
pub const SYSTEM: u16 = 4;
pub const HARD_LINK: u16 = 8;
pub const REPARSE_POINT: u16 = 16;
pub const RECORD_MASK: u64 = 0x0000_ffff_ffff_ffff;
pub const VOLUME_TRAVERSAL: u16 = 1;
pub const VOLUME_PARTIAL: u16 = 2;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MftNode {
    pub file_ref_number: u64,
    pub logical_size: u64,
    pub allocated_size: u64,
    pub modified_time_ns100: i64,
    pub parent_index: u32,
    pub name_offset: u32,
    pub name_len: u16,
    pub flags: u16,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MftVolume {
    pub volume_serial: u64,
    pub usn_journal_id: u64,
    pub last_usn: i64,
    pub root_frn: u64,
    pub first_node_index: u32,
    pub node_count: u32,
    pub drive_letter: u16,
    pub flags: u16,
    pub reserved: u32,
}

#[derive(Clone, Debug, Default)]
pub struct MftTree {
    pub volumes: Vec<MftVolume>,
    pub nodes: Vec<MftNode>,
    pub strings: Vec<u8>,
}

impl MftTree {
    pub fn name(&self, index: usize) -> &str {
        let Some(node) = self.nodes.get(index) else {
            return "";
        };
        let start = node.name_offset as usize;
        self.strings
            .get(start..start + usize::from(node.name_len))
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .unwrap_or("")
    }

    pub fn path(&self, index: usize) -> PathBuf {
        let mut parts = Vec::new();
        let mut current = index;
        for _ in 0..self.nodes.len() {
            let Some(node) = self.nodes.get(current) else {
                return PathBuf::new();
            };
            parts.push(self.name(current));
            if node.parent_index == u32::MAX {
                return parts.into_iter().rev().collect();
            }
            current = node.parent_index as usize;
        }
        PathBuf::new()
    }

    /// One-to-many references deliberately retain every hard-link path.
    pub fn reference_index(&self, volume_index: usize) -> HashMap<u64, Vec<usize>> {
        let mut result = HashMap::<u64, Vec<usize>>::new();
        if let Some(volume) = self.volumes.get(volume_index) {
            let start = volume.first_node_index as usize;
            let end = start
                .saturating_add(volume.node_count as usize)
                .min(self.nodes.len());
            for (index, node) in self.nodes.iter().enumerate().take(end).skip(start) {
                result.entry(node.file_ref_number).or_default().push(index);
            }
        }
        result
    }

    /// Each physical record is charged to its first path, consistently across ancestors.
    /// File sizes remain intact on every link for search and per-file inspection.
    pub fn aggregate(&mut self) {
        let mut pending = vec![0usize; self.nodes.len()];
        let mut logical = vec![0u64; self.nodes.len()];
        let mut allocated = vec![0u64; self.nodes.len()];
        for volume in &self.volumes {
            let mut seen = HashSet::new();
            let start = volume.first_node_index as usize;
            let end = start
                .saturating_add(volume.node_count as usize)
                .min(self.nodes.len());
            for index in start..end {
                let node = &mut self.nodes[index];
                if node.flags & DIRECTORY != 0 {
                    node.logical_size = 0;
                    node.allocated_size = 0;
                } else if seen.insert(node.file_ref_number & RECORD_MASK) {
                    logical[index] = node.logical_size;
                    allocated[index] = node.allocated_size;
                }
                let parent = node.parent_index as usize;
                if (start..end).contains(&parent) && parent != index {
                    pending[parent] += 1;
                }
            }
        }
        let mut leaves: VecDeque<usize> = pending
            .iter()
            .enumerate()
            .filter_map(|(index, count)| (*count == 0).then_some(index))
            .collect();
        while let Some(index) = leaves.pop_front() {
            let node = self.nodes[index];
            if node.flags & DIRECTORY != 0 {
                self.nodes[index].logical_size = logical[index];
                self.nodes[index].allocated_size = allocated[index];
            }
            let parent = node.parent_index as usize;
            if parent >= self.nodes.len() || parent == index || pending[parent] == 0 {
                continue;
            }
            logical[parent] = logical[parent].saturating_add(logical[index]);
            allocated[parent] = allocated[parent].saturating_add(allocated[index]);
            self.nodes[parent].modified_time_ns100 = self.nodes[parent]
                .modified_time_ns100
                .max(node.modified_time_ns100);
            pending[parent] -= 1;
            if pending[parent] == 0 {
                leaves.push_back(parent);
            }
        }
    }

    pub(crate) fn replace_volume(&mut self, replacement: MftTree) -> Result<(), String> {
        let serial = replacement
            .volumes
            .first()
            .ok_or("scan returned no volume")?
            .volume_serial;
        let mut rebuilt = Self::default();
        for volume in &self.volumes {
            if volume.volume_serial != serial {
                rebuilt.append_volume(self, *volume)?;
            }
        }
        for volume in &replacement.volumes {
            rebuilt.append_volume(&replacement, *volume)?;
        }
        *self = rebuilt;
        Ok(())
    }

    fn append_volume(&mut self, source: &Self, mut volume: MftVolume) -> Result<(), String> {
        let old_start = volume.first_node_index as usize;
        let new_start = u32::try_from(self.nodes.len()).map_err(|_| "too many MFT nodes")?;
        let end = old_start
            .checked_add(volume.node_count as usize)
            .ok_or("invalid volume range")?;
        let nodes = source
            .nodes
            .get(old_start..end)
            .ok_or("invalid volume range")?;
        for (offset, old) in nodes.iter().enumerate() {
            let mut node = *old;
            node.name_offset =
                u32::try_from(self.strings.len()).map_err(|_| "MFT string pool exceeds 4 GiB")?;
            self.strings
                .extend_from_slice(source.name(old_start + offset).as_bytes());
            if node.parent_index != u32::MAX {
                node.parent_index = node
                    .parent_index
                    .checked_sub(volume.first_node_index)
                    .and_then(|relative| relative.checked_add(new_start))
                    .ok_or("invalid parent index")?;
            }
            self.nodes.push(node);
        }
        volume.first_node_index = new_start;
        self.volumes.push(volume);
        Ok(())
    }

    fn retain_volumes(&mut self, serials: &HashSet<u64>) -> Result<(), String> {
        let mut rebuilt = Self::default();
        for volume in &self.volumes {
            if serials.contains(&volume.volume_serial) {
                rebuilt.append_volume(self, *volume)?;
            }
        }
        *self = rebuilt;
        Ok(())
    }
}

pub struct MftEngine {
    snapshot_path: PathBuf,
    config: FileSearchConfig,
    tree: Option<MftTree>,
    consumers: usize,
    idle_since: Option<Instant>,
    last_poll: Option<Instant>,
    degraded: bool,
    search_requested: bool,
    warnings: Vec<String>,
    revision: u64,
}

impl MftEngine {
    pub fn new(snapshot_path: PathBuf, config: FileSearchConfig) -> Self {
        Self {
            snapshot_path,
            config,
            tree: None,
            consumers: 0,
            idle_since: None,
            last_poll: None,
            degraded: false,
            search_requested: false,
            warnings: Vec::new(),
            revision: 0,
        }
    }

    pub fn configure(&mut self, config: FileSearchConfig) {
        self.config = config;
        self.last_poll = None;
        if self.reference_count() == 0 {
            self.idle_since.get_or_insert_with(Instant::now);
            if self.effective_mode() == "OnDemandOnly" {
                self.unload();
            }
        } else {
            self.idle_since = None;
        }
    }

    pub fn consumers(&self) -> usize {
        self.consumers
    }
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
    pub fn tree(&self) -> Option<&MftTree> {
        self.tree.as_ref()
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn effective_mode(&self) -> &str {
        if self.degraded || !self.config.enabled || self.config.backend != "BuiltinMft" {
            "OnDemandOnly"
        } else {
            &self.config.residency_mode
        }
    }

    fn reference_count(&self) -> usize {
        self.consumers + usize::from(self.effective_mode() == "Resident")
    }

    pub fn acquire(&mut self) -> Result<(), String> {
        // Select search or target before loading, so one acquisition needs only one refresh.
        self.consumers = self
            .consumers
            .checked_add(1)
            .ok_or("MFT consumer count overflow")?;
        self.idle_since = None;
        Ok(())
    }

    pub fn next_tick_in(&self) -> Option<Duration> {
        if self.reference_count() > 0 {
            return Some(self.last_poll.map_or(Duration::ZERO, |last| {
                Duration::from_secs(1).saturating_sub(last.elapsed())
            }));
        }
        self.tree.as_ref()?;
        if self.effective_mode() == "OnDemandOnly" {
            return Some(Duration::ZERO);
        }
        Some(self.idle_since.map_or(Duration::ZERO, |since| {
            Duration::from_secs(u64::from(self.config.idle_release_seconds))
                .saturating_sub(since.elapsed())
        }))
    }

    fn refresh_due(&mut self) -> Result<(), String> {
        if self
            .last_poll
            .is_none_or(|last| last.elapsed() >= Duration::from_secs(1))
        {
            self.last_poll = Some(Instant::now());
            self.refresh()?;
        }
        Ok(())
    }

    pub fn release(&mut self) -> Result<(), String> {
        self.consumers = self
            .consumers
            .checked_sub(1)
            .ok_or("MFT consumer count underflow")?;
        if self.reference_count() == 0 {
            self.idle_since = Some(Instant::now());
            if self.effective_mode() == "OnDemandOnly" {
                self.unload();
            }
        }
        Ok(())
    }

    pub fn tick(&mut self) -> Result<(), String> {
        if self.effective_mode() == "Resident" {
            self.search_requested = true;
        }
        if self.reference_count() > 0 && self.tree.is_none() {
            self.last_poll = Some(Instant::now());
            return self.load();
        }
        if self.reference_count() == 0 {
            let since = *self.idle_since.get_or_insert_with(Instant::now);
            if self.effective_mode() == "OnDemandOnly" {
                self.unload();
                return Ok(());
            }
            if since.elapsed() >= Duration::from_secs(u64::from(self.config.idle_release_seconds))
                && let Some(tree) = self.tree.as_ref()
            {
                snapshot::save(&self.snapshot_path, tree).map_err(|error| error.to_string())?;
                self.unload();
            }
            return Ok(());
        }
        if self.tree.is_some() {
            self.refresh_due()?;
        }
        Ok(())
    }

    pub fn ensure_search(&mut self) -> Result<(), String> {
        let was_requested = self.search_requested;
        let loaded = self.tree.is_some();
        self.search_requested = true;
        if let Err(error) = self.load() {
            self.search_requested = was_requested;
            return Err(error);
        }
        if loaded {
            if !was_requested {
                self.last_poll = None;
            }
            self.refresh_due()?;
        }
        if self
            .tree
            .as_ref()
            .is_none_or(|tree| tree.volumes.is_empty())
        {
            return Err(format!(
                "No accessible mounted volumes: {}",
                self.warnings.join("; ")
            ));
        }
        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<(), String> {
        if self.effective_mode() != "OnDemandOnly"
            && let Some(tree) = self.tree.as_ref()
        {
            snapshot::save(&self.snapshot_path, tree).map_err(|error| error.to_string())?;
        }
        self.unload();
        Ok(())
    }

    fn unload(&mut self) {
        if self.tree.take().is_some() {
            self.revision = self.revision.wrapping_add(1);
        }
        self.search_requested = false;
        self.last_poll = None;
    }

    pub fn ensure_target(&mut self, path: &Path) -> Result<(), String> {
        self.ensure_target_with_refresh(path, false)
    }

    /// Rescan cached traversal coverage on explicit analysis requests, never on polling ticks.
    pub fn refresh_target(&mut self, path: &Path) -> Result<(), String> {
        self.ensure_target_with_refresh(path, true)
    }

    fn ensure_target_with_refresh(&mut self, path: &Path, refresh: bool) -> Result<(), String> {
        self.load()?;
        self.refresh_due()?;
        let absolute = std::path::absolute(path).map_err(|error| error.to_string())?;
        let volume = ntfs_scanner::volume_for_path(&absolute)?;
        self.ensure_volume_target(&volume, absolute, refresh)
    }

    fn ensure_volume_target(
        &mut self,
        volume: &ntfs_scanner::VolumeInfo,
        mut target: PathBuf,
        refresh: bool,
    ) -> Result<(), String> {
        if let Some(tree) = self.tree.as_ref()
            && let Some(previous) = tree
                .volumes
                .iter()
                .find(|candidate| candidate.volume_serial == volume.serial)
        {
            let root = tree.path(previous.first_node_index as usize);
            if previous.flags & VOLUME_TRAVERSAL == 0 || (!refresh && path_within(&target, &root)) {
                return Ok(());
            }
            // Preserve coverage needed by other consumers when refreshing a nested target.
            target = common_root(&root, &target);
        }
        let scanned = ntfs_scanner::scan_target(volume, &target)?;
        self.accept_scan(scanned)?;
        if self.reference_count() == 0 {
            self.idle_since = Some(Instant::now());
        }
        Ok(())
    }

    fn accept_scan(&mut self, scan: ntfs_scanner::ScanResult) -> Result<(), String> {
        self.degraded |= scan
            .tree
            .volumes
            .iter()
            .any(|volume| volume.flags & VOLUME_TRAVERSAL != 0);
        self.warnings.extend(scan.warnings);
        self.tree
            .get_or_insert_with(MftTree::default)
            .replace_volume(scan.tree)?;
        self.revision = self.revision.wrapping_add(1);
        Ok(())
    }

    fn load(&mut self) -> Result<(), String> {
        if self.tree.is_some() {
            return Ok(());
        }
        // Publish only a complete initial load; a failed refresh must remain retryable.
        let mut staged = Self::new(self.snapshot_path.clone(), self.config.clone());
        staged.consumers = self.consumers;
        staged.idle_since = self.idle_since;
        staged.search_requested = self.search_requested;
        staged.revision = self.revision;
        staged.load_initial()?;
        *self = staged;
        Ok(())
    }

    fn load_initial(&mut self) -> Result<(), String> {
        self.warnings.clear();
        self.degraded = false;
        if self.effective_mode() != "OnDemandOnly" {
            match snapshot::load(&self.snapshot_path) {
                Ok(tree)
                    if tree
                        .volumes
                        .iter()
                        .any(|volume| volume.flags & VOLUME_TRAVERSAL != 0) =>
                {
                    self.degraded = true;
                    self.warnings.push(
                        "Traversal-only snapshot discarded: live OnDemandOnly scan required".into(),
                    );
                }
                Ok(tree) => {
                    self.tree = Some(tree);
                    self.revision = self.revision.wrapping_add(1);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => self
                    .warnings
                    .push(format!("MFT snapshot discarded: {error}")),
            }
        }
        self.tree.get_or_insert_with(MftTree::default);
        self.refresh()?;
        self.last_poll = Some(Instant::now());
        Ok(())
    }

    fn refresh(&mut self) -> Result<(), String> {
        let mounted = ntfs_scanner::mounted_volumes()?;
        let serials: HashSet<_> = mounted.iter().map(|volume| volume.serial).collect();
        if let Some(tree) = self.tree.as_mut()
            && tree
                .volumes
                .iter()
                .any(|volume| !serials.contains(&volume.volume_serial))
        {
            tree.retain_volumes(&serials)?;
            self.revision = self.revision.wrapping_add(1);
        }
        for volume in mounted {
            let previous = self
                .tree
                .as_ref()
                .and_then(|tree| {
                    tree.volumes
                        .iter()
                        .find(|candidate| candidate.volume_serial == volume.serial)
                })
                .copied();
            let result = match previous {
                Some(previous) if previous.flags & VOLUME_TRAVERSAL != 0 => {
                    let Some(target) = self.search_target(&volume) else {
                        continue;
                    };
                    let root = self
                        .tree
                        .as_ref()
                        .unwrap()
                        .path(previous.first_node_index as usize);
                    if path_within(&target, &root) {
                        continue;
                    }
                    ntfs_scanner::scan_target(&volume, &common_root(&root, &target)).map(Some)
                }
                Some(previous) => {
                    usn_watcher::refresh(&volume, self.tree.as_ref().unwrap(), previous)
                }
                None if self.search_requested => {
                    let Some(target) = self.search_target(&volume) else {
                        continue;
                    };
                    ntfs_scanner::scan_target(&volume, &target).map(Some)
                }
                None => continue,
            };
            match result {
                Ok(Some(scan)) => self.accept_scan(scan)?,
                Ok(None) => {}
                Err(error) => {
                    let message = format!("{}: {error}", volume.root.display());
                    if !self.warnings.contains(&message) {
                        self.warnings.push(message);
                    }
                }
            }
        }
        Ok(())
    }

    fn search_target(&self, volume: &ntfs_scanner::VolumeInfo) -> Option<PathBuf> {
        if !self.search_requested {
            return None;
        }
        if self.config.included_drives.is_empty() {
            return Some(volume.root.clone());
        }
        let mut root = None::<PathBuf>;
        for included in &self.config.included_drives {
            let text = included.trim();
            let path = if text.len() == 1 && text.as_bytes()[0].is_ascii_alphabetic() {
                PathBuf::from(format!("{text}:\\"))
            } else if text.len() == 2 && text.ends_with(':') {
                PathBuf::from(format!("{text}\\"))
            } else {
                PathBuf::from(text)
            };
            if let Ok(candidate) = ntfs_scanner::volume_for_path(&path)
                && candidate.serial == volume.serial
            {
                root = Some(match root {
                    Some(previous) => common_root(&previous, &path),
                    None => path,
                });
            }
        }
        root
    }
}

fn path_within(path: &Path, root: &Path) -> bool {
    let path = path.to_string_lossy().replace('/', "\\").to_lowercase();
    let root = root.to_string_lossy().replace('/', "\\").to_lowercase();
    let root = root.trim_end_matches('\\');
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('\\'))
}

fn common_root(left: &Path, right: &Path) -> PathBuf {
    let mut root = left.to_path_buf();
    while !path_within(right, &root) && root.pop() {}
    root
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_traversal_refresh_updates_files_and_preserves_coverage() {
        let root = std::env::temp_dir().join(format!(
            "orbment-mft-refresh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let removed = root.join("removed");
        let changed = nested.join("changed");
        let added = root.join("added");
        std::fs::write(&removed, b"old").unwrap();
        std::fs::write(&changed, b"before").unwrap();

        let mut volume = ntfs_scanner::volume_for_path(&root).unwrap();
        // Exercise traversal even when tests run elevated on an NTFS volume.
        volume.ntfs = false;
        let mut engine = MftEngine::new(PathBuf::new(), FileSearchConfig::default());
        engine
            .ensure_volume_target(&volume, root.clone(), true)
            .unwrap();
        assert_eq!(engine.revision(), 1);
        assert_ne!(
            engine.tree().unwrap().volumes[0].flags & VOLUME_TRAVERSAL,
            0
        );

        std::fs::remove_file(&removed).unwrap();
        std::fs::write(&changed, b"after refresh").unwrap();
        std::fs::write(&added, b"new").unwrap();
        engine
            .ensure_volume_target(&volume, root.clone(), false)
            .unwrap();
        assert_eq!(engine.revision(), 1);
        let tree = engine.tree().unwrap();
        assert!((0..tree.nodes.len()).any(|index| tree.path(index) == removed));
        assert!(!(0..tree.nodes.len()).any(|index| tree.path(index) == added));

        engine
            .ensure_volume_target(&volume, root.clone(), true)
            .unwrap();
        assert_eq!(engine.revision(), 2);
        let tree = engine.tree().unwrap();
        assert!(!(0..tree.nodes.len()).any(|index| tree.path(index) == removed));
        assert!((0..tree.nodes.len()).any(|index| tree.path(index) == added));
        let index = (0..tree.nodes.len())
            .find(|&index| tree.path(index) == changed)
            .unwrap();
        assert_eq!(tree.nodes[index].logical_size, 13);

        std::fs::remove_file(&added).unwrap();
        engine.ensure_volume_target(&volume, nested, true).unwrap();
        assert_eq!(engine.revision(), 3);
        let tree = engine.tree().unwrap();
        assert_eq!(tree.path(tree.volumes[0].first_node_index as usize), root);
        assert!(!(0..tree.nodes.len()).any(|index| tree.path(index) == added));
        assert!((0..tree.nodes.len()).any(|index| tree.path(index) == changed));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_target_refresh_leaves_raw_ntfs_scans_to_usn() {
        let root = PathBuf::from("C:\\");
        let volume = ntfs_scanner::VolumeInfo {
            root: root.clone(),
            device: PathBuf::new(),
            serial: 42,
            ntfs: true,
            drive_letter: u16::from(b'C'),
        };
        let mut engine = MftEngine::new(PathBuf::new(), FileSearchConfig::default());
        engine.tree = Some(MftTree {
            volumes: vec![MftVolume {
                volume_serial: volume.serial,
                node_count: 1,
                ..Default::default()
            }],
            nodes: vec![MftNode {
                parent_index: u32::MAX,
                name_len: 3,
                flags: DIRECTORY,
                ..Default::default()
            }],
            strings: b"C:\\".to_vec(),
        });
        engine
            .ensure_volume_target(&volume, root.join("target"), true)
            .unwrap();
        assert_eq!(engine.revision(), 0);
        assert_eq!(engine.tree().unwrap().volumes[0].flags, 0);
    }

    #[test]
    fn unloaded_engine_has_no_idle_polling_deadline() {
        let mut engine = MftEngine::new(
            PathBuf::new(),
            FileSearchConfig {
                enabled: true,
                backend: "BuiltinMft".into(),
                residency_mode: "OnDemandOnly".into(),
                ..Default::default()
            },
        );
        assert_eq!(engine.next_tick_in(), None);
        engine.acquire().unwrap();
        assert_eq!(engine.next_tick_in(), Some(Duration::ZERO));
        engine.release().unwrap();
        assert_eq!(engine.next_tick_in(), None);

        engine.configure(FileSearchConfig {
            enabled: true,
            backend: "BuiltinMft".into(),
            residency_mode: "Resident".into(),
            ..Default::default()
        });
        assert_eq!(engine.next_tick_in(), Some(Duration::ZERO));
        engine.configure(FileSearchConfig {
            enabled: false,
            ..Default::default()
        });
        assert_eq!(engine.next_tick_in(), None);
    }

    #[test]
    fn snapshot_waits_for_last_consumer_then_saves_without_idle_polling() {
        let path = std::env::temp_dir().join(format!(
            "orbment-mft-idle-{}-{}.snapshot",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut engine = MftEngine::new(
            path.clone(),
            FileSearchConfig {
                enabled: true,
                backend: "BuiltinMft".into(),
                residency_mode: "SnapshotIncrement".into(),
                idle_release_seconds: 60,
                ..Default::default()
            },
        );
        engine.tree = Some(MftTree::default());
        engine.last_poll = Some(Instant::now());
        engine.acquire().unwrap();
        engine.acquire().unwrap();
        engine.release().unwrap();
        assert!(engine.next_tick_in().unwrap() <= Duration::from_secs(1));
        assert!(engine.tree().is_some());
        engine.release().unwrap();
        assert!(engine.next_tick_in().unwrap() > Duration::from_secs(59));
        engine.idle_since = Some(Instant::now() - Duration::from_secs(60));
        engine.tick().unwrap();
        assert!(engine.tree().is_none());
        assert_eq!(engine.next_tick_in(), None);
        assert!(snapshot::load(&path).unwrap().volumes.is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn hardlinks_keep_paths_but_are_charged_once() {
        let mut tree = MftTree {
            volumes: vec![MftVolume {
                node_count: 4,
                ..Default::default()
            }],
            nodes: vec![
                MftNode {
                    file_ref_number: 5,
                    parent_index: u32::MAX,
                    name_len: 3,
                    flags: DIRECTORY,
                    ..Default::default()
                },
                MftNode {
                    file_ref_number: 6,
                    parent_index: 0,
                    name_offset: 3,
                    name_len: 1,
                    flags: DIRECTORY,
                    ..Default::default()
                },
                MftNode {
                    file_ref_number: 7,
                    parent_index: 0,
                    name_offset: 4,
                    name_len: 1,
                    logical_size: 9,
                    allocated_size: 16,
                    flags: HARD_LINK,
                    ..Default::default()
                },
                MftNode {
                    file_ref_number: 7,
                    parent_index: 1,
                    name_offset: 5,
                    name_len: 1,
                    logical_size: 9,
                    allocated_size: 16,
                    flags: HARD_LINK,
                    ..Default::default()
                },
            ],
            strings: b"C:\\dab".to_vec(),
        };
        tree.aggregate();
        assert_eq!(tree.nodes[0].allocated_size, 16);
        assert_eq!(tree.nodes[1].allocated_size, 0);
        assert_eq!(tree.reference_index(0)[&7], vec![2, 3]);
        assert!(tree.path(3).ends_with("b"));
        tree.aggregate();
        assert_eq!(tree.nodes[0].allocated_size, 16);
    }
}
