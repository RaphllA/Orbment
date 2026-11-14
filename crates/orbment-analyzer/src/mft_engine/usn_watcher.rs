use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io;

use super::ntfs_scanner::{
    self, Entry, RawVolume, ScanResult, VolumeInfo, build_tree, control, tree_entries,
};
use super::{DIRECTORY, MftTree, MftVolume, RECORD_MASK};

const QUERY_USN_JOURNAL: u32 = 0x0009_00f4;
const READ_USN_JOURNAL: u32 = 0x0009_00bb;

#[derive(Clone, Copy, Debug)]
pub(super) struct Journal {
    pub id: u64,
    pub first: i64,
    pub next: i64,
    pub lowest: i64,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

pub(super) fn query(file: &File) -> io::Result<Journal> {
    let mut output = [0u8; 80];
    let returned = control(file, QUERY_USN_JOURNAL, &[], &mut output)?;
    if returned < 56 {
        return Err(invalid("short USN_JOURNAL_DATA response"));
    }
    Ok(Journal {
        id: u64_at(&output, 0),
        first: u64_at(&output, 8) as i64,
        next: u64_at(&output, 16) as i64,
        lowest: u64_at(&output, 24) as i64,
    })
}

fn changes(file: &File, previous: Journal) -> io::Result<(Journal, HashSet<u64>)> {
    let target = query(file)?;
    if target.id != previous.id
        || previous.next < target.first.max(target.lowest)
        || previous.next > target.next
    {
        return Err(invalid(
            "USN journal replaced, truncated, or cursor invalid; full rescan required",
        ));
    }
    let mut cursor = previous.next;
    let mut changed = HashSet::new();
    let mut output = vec![0u8; 1024 * 1024];
    while cursor < target.next {
        // V0: StartUsn, ReasonMask, ReturnOnlyOnClose, Timeout, BytesToWaitFor, JournalId.
        // BytesToWaitFor=0 and Timeout=0 keep the worker's poll nonblocking.
        let mut input = [0u8; 40];
        input[0..8].copy_from_slice(&cursor.to_le_bytes());
        input[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        input[32..40].copy_from_slice(&target.id.to_le_bytes());
        let returned = control(file, READ_USN_JOURNAL, &input, &mut output)?;
        if returned < 8 {
            return Err(invalid("short READ_USN_JOURNAL response"));
        }
        let next = u64_at(&output, 0) as i64;
        if next <= cursor {
            return Err(invalid("USN journal cursor made no progress"));
        }
        let mut offset = 8;
        while offset < returned {
            if returned - offset < 8 {
                return Err(invalid("truncated USN record header"));
            }
            let length = u32_at(&output, offset) as usize;
            if length < 8 || !length.is_multiple_of(8) || length > returned - offset {
                return Err(invalid("invalid USN record length"));
            }
            let record = &output[offset..offset + length];
            let (frn, usn, name_length, name_offset) = match u16_at(record, 4) {
                2 if length >= 60 => (
                    u64_at(record, 8),
                    u64_at(record, 24) as i64,
                    u16_at(record, 56) as usize,
                    u16_at(record, 58) as usize,
                ),
                3 if length >= 76 && u64_at(record, 16) == 0 => (
                    u64_at(record, 8),
                    u64_at(record, 40) as i64,
                    u16_at(record, 72) as usize,
                    u16_at(record, 74) as usize,
                ),
                _ => return Err(invalid("unsupported USN record version/file reference")),
            };
            if name_length % 2 != 0
                || name_offset
                    .checked_add(name_length)
                    .is_none_or(|end| end > length)
            {
                return Err(invalid("invalid USN filename range"));
            }
            // Do not consume beyond the captured boundary: later writes remain for the next poll.
            if usn >= previous.next && usn < target.next {
                changed.insert(frn & RECORD_MASK);
            }
            offset += length;
        }
        cursor = next;
    }
    let after = query(file)?;
    if after.id != target.id || previous.next < after.first.max(after.lowest) {
        return Err(invalid(
            "USN journal changed while replaying; full rescan required",
        ));
    }
    Ok((target, changed))
}

pub(super) fn catch_up(
    volume: &VolumeInfo,
    raw: &mut RawVolume,
    mut entries: Vec<Entry>,
    previous: Journal,
) -> Result<MftTree, String> {
    let (cursor, changed) =
        changes(&raw.file, previous).map_err(|error| format!("Read USN journal: {error}"))?;
    apply(&mut entries, changed, |number| raw.read_entry(number))?;
    build_tree(volume, entries, Some(cursor), 0)
}

fn apply(
    entries: &mut Vec<Entry>,
    changed: HashSet<u64>,
    mut read_entry: impl FnMut(u64) -> io::Result<Vec<Entry>>,
) -> Result<(), String> {
    if changed.is_empty() {
        return Ok(());
    }
    let mut old_parents = HashMap::<u64, Vec<u64>>::new();
    for entry in entries.iter() {
        if entry.parent != entry.frn
            && (entry.flags & DIRECTORY != 0 || changed.contains(&(entry.frn & RECORD_MASK)))
        {
            old_parents
                .entry(entry.frn & RECORD_MASK)
                .or_default()
                .push(entry.parent & RECORD_MASK);
        }
    }
    let mut pending: VecDeque<_> = changed.into_iter().collect();
    let mut refreshed = HashSet::new();
    let mut replacement_rows = Vec::new();
    while let Some(number) = pending.pop_front() {
        if !refreshed.insert(number) {
            continue;
        }
        if let Some(parents) = old_parents.get(&number) {
            pending.extend(parents);
        }
        let rows =
            read_entry(number).map_err(|error| format!("Refresh MFT record {number}: {error}"))?;
        for row in &rows {
            if row.parent != row.frn {
                pending.push_back(row.parent & RECORD_MASK);
            }
        }
        replacement_rows.extend(rows);
    }
    // Refresh ancestors' own timestamps before aggregation, so deleting the newest
    // child can decrease a directory's aggregate timestamp. All hard-link paths move together.
    entries.retain(|entry| !refreshed.contains(&(entry.frn & RECORD_MASK)));
    entries.extend(replacement_rows);
    Ok(())
}

pub(super) fn refresh(
    volume: &VolumeInfo,
    tree: &MftTree,
    previous: MftVolume,
) -> Result<Option<ScanResult>, String> {
    let mut raw = RawVolume::open(volume).map_err(|error| format!("Open USN volume: {error}"))?;
    let start = Journal {
        id: previous.usn_journal_id,
        first: 0,
        next: previous.last_usn,
        lowest: 0,
    };
    let (cursor, changed) = match changes(&raw.file, start) {
        Ok(changes) => changes,
        Err(error) => {
            let mut scan = ntfs_scanner::scan(volume)?;
            scan.warnings.push(format!(
                "{}: USN cursor invalid; volume rescanned ({error})",
                volume.root.display()
            ));
            return Ok(Some(scan));
        }
    };
    if changed.is_empty()
        && cursor.next == previous.last_usn
        && previous.drive_letter == volume.drive_letter
        && tree.path(previous.first_node_index as usize) == volume.root
    {
        return Ok(None);
    }
    let mut entries = tree_entries(tree, previous)?;
    apply(&mut entries, changed, |number| raw.read_entry(number))?;
    let replacement = build_tree(volume, entries, Some(cursor), previous.flags)?;
    Ok(Some(ScanResult {
        tree: replacement,
        warnings: Vec::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::super::HARD_LINK;
    use super::*;

    fn volume() -> VolumeInfo {
        VolumeInfo {
            root: r"C:\".into(),
            device: r"\\.\C:".into(),
            serial: 42,
            ntfs: true,
            drive_letter: u16::from(b'C'),
        }
    }

    fn entry(number: u64, parent: u64, name: &str, flags: u16, modified: i64) -> Entry {
        Entry {
            frn: (1 << 48) | number,
            parent: (1 << 48) | parent,
            name: name.into(),
            logical: if flags & DIRECTORY == 0 { 8 } else { 0 },
            allocated: if flags & DIRECTORY == 0 { 16 } else { 0 },
            modified,
            flags,
        }
    }

    #[test]
    fn changes_replace_all_links_and_refresh_old_and_new_ancestor_timestamps() {
        let volume = volume();
        let root = entry(5, 5, "", DIRECTORY, 1);
        let old_parent = entry(10, 5, "old", DIRECTORY, 10);
        let new_parent = entry(20, 5, "new", DIRECTORY, 20);
        let snapshot = build_tree(
            &volume,
            vec![
                root.clone(),
                old_parent.clone(),
                new_parent.clone(),
                entry(30, 10, "first", HARD_LINK, 100),
                entry(30, 10, "second", HARD_LINK, 100),
                entry(40, 20, "deleted", 0, 200),
            ],
            None,
            0,
        )
        .unwrap();
        let mut entries = tree_entries(&snapshot, snapshot.volumes[0]).unwrap();
        let mut records = HashMap::from([
            (5, vec![root]),
            (10, vec![old_parent]),
            (20, vec![new_parent]),
            (
                30,
                vec![
                    entry(30, 20, "renamed-first", HARD_LINK, 120),
                    entry(30, 20, "renamed-second", HARD_LINK, 120),
                ],
            ),
            (40, Vec::new()),
        ]);
        apply(&mut entries, HashSet::from([30, 40]), |number| {
            Ok(records.remove(&number).expect("each ancestor is read once"))
        })
        .unwrap();
        assert!(records.is_empty());
        let tree = build_tree(&volume, entries, None, 0).unwrap();
        let find = |name| {
            tree.nodes
                .iter()
                .enumerate()
                .find(|(index, _)| tree.name(*index) == name)
                .unwrap()
        };
        assert_eq!(tree.nodes.len(), 5);
        assert_eq!(tree.nodes[0].logical_size, 8);
        assert_eq!(tree.nodes[0].allocated_size, 16);
        assert_eq!(tree.nodes[0].modified_time_ns100, 120);
        let (_, old) = find("old");
        assert_eq!(old.logical_size, 0);
        assert_eq!(old.modified_time_ns100, 10);
        let (_, new) = find("new");
        assert_eq!(new.logical_size, 8);
        assert_eq!(new.modified_time_ns100, 120);
        for name in ["renamed-first", "renamed-second"] {
            let (index, node) = find(name);
            assert_eq!(tree.path(index), volume.root.join("new").join(name));
            assert_eq!(node.logical_size, 8);
            assert_eq!(node.allocated_size, 16);
        }
    }

    #[test]
    fn new_parent_is_merged_before_resolving_scanned_children() {
        let volume = volume();
        let root = entry(5, 5, "", DIRECTORY, 1);
        let mut entries = vec![root.clone(), entry(30, 20, "child", 0, 100)];
        let mut records = HashMap::from([
            (5, vec![root]),
            (20, vec![entry(20, 5, "created-during-scan", DIRECTORY, 20)]),
        ]);
        apply(&mut entries, HashSet::from([20]), |number| {
            Ok(records.remove(&number).unwrap())
        })
        .unwrap();
        let tree = build_tree(&volume, entries, None, 0).unwrap();
        let child = tree
            .nodes
            .iter()
            .position(|node| node.file_ref_number & RECORD_MASK == 30)
            .unwrap();
        assert_eq!(
            tree.path(child),
            volume.root.join("created-during-scan").join("child")
        );
        assert_eq!(tree.nodes[0].logical_size, 8);
    }

    #[test]
    fn empty_changes_do_not_read_mft_records() {
        let volume = volume();
        let mut entries = vec![entry(5, 5, "", DIRECTORY, 10)];
        apply(&mut entries, HashSet::new(), |_| {
            panic!("no changed record")
        })
        .unwrap();
        let cursor = Journal {
            id: 7,
            first: 0,
            next: 123,
            lowest: 0,
        };
        let tree = build_tree(&volume, entries, Some(cursor), 0).unwrap();
        assert_eq!(tree.volumes[0].last_usn, 123);
        assert_eq!(tree.volumes[0].usn_journal_id, 7);
        assert_eq!(tree.nodes[0].modified_time_ns100, 10);
    }
}
