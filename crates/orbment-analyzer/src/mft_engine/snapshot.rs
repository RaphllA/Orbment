use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{MftNode, MftTree, MftVolume};

const WIDTH: u64 = 48;
const VERSION: u32 = 2;
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, PartialEq, Eq)]
pub struct Header {
    pub volume_count: u32,
    pub total_nodes: u64,
    pub string_pool_bytes: u64,
    pub written_at_unix_ms: u64,
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

/// Header validation is constant-time; cache contents are never hashed.
pub fn read_header(mut reader: impl Read, file_length: u64) -> io::Result<Header> {
    let mut bytes = [0u8; 48];
    reader.read_exact(&mut bytes)?;
    if &bytes[..8] != b"ORBMFT01" || u32_at(&bytes, 8) != VERSION {
        return Err(invalid("unsupported MFT snapshot magic/version"));
    }
    let header = Header {
        volume_count: u32_at(&bytes, 12),
        total_nodes: u64_at(&bytes, 16),
        string_pool_bytes: u64_at(&bytes, 24),
        written_at_unix_ms: u64_at(&bytes, 32),
    };
    let expected = u64::from(header.volume_count)
        .checked_add(header.total_nodes)
        .and_then(|count| count.checked_mul(WIDTH))
        .and_then(|bytes| bytes.checked_add(WIDTH))
        .and_then(|bytes| bytes.checked_add(header.string_pool_bytes));
    if expected != Some(file_length) {
        return Err(invalid("MFT snapshot length/count mismatch"));
    }
    Ok(header)
}

pub fn load(path: &Path) -> io::Result<MftTree> {
    let file = File::open(path)?;
    let length = file.metadata()?.len();
    decode(BufReader::new(file), length)
}

fn decode(mut reader: impl Read, length: u64) -> io::Result<MftTree> {
    let header = read_header(&mut reader, length)?;
    let mut tree = MftTree::default();
    tree.volumes
        .try_reserve_exact(header.volume_count as usize)
        .map_err(|error| invalid(&error.to_string()))?;
    tree.nodes
        .try_reserve_exact(header.total_nodes as usize)
        .map_err(|error| invalid(&error.to_string()))?;
    tree.strings
        .try_reserve_exact(header.string_pool_bytes as usize)
        .map_err(|error| invalid(&error.to_string()))?;
    let mut bytes = [0u8; 48];
    for _ in 0..header.volume_count {
        reader.read_exact(&mut bytes)?;
        tree.volumes.push(MftVolume {
            volume_serial: u64_at(&bytes, 0),
            usn_journal_id: u64_at(&bytes, 8),
            last_usn: u64_at(&bytes, 16) as i64,
            root_frn: u64_at(&bytes, 24),
            first_node_index: u32_at(&bytes, 32),
            node_count: u32_at(&bytes, 36),
            drive_letter: u16_at(&bytes, 40),
            flags: u16_at(&bytes, 42),
            reserved: 0,
        });
    }
    for _ in 0..header.total_nodes {
        reader.read_exact(&mut bytes)?;
        tree.nodes.push(MftNode {
            file_ref_number: u64_at(&bytes, 0),
            logical_size: u64_at(&bytes, 8),
            allocated_size: u64_at(&bytes, 16),
            modified_time_ns100: u64_at(&bytes, 24) as i64,
            parent_index: u32_at(&bytes, 32),
            name_offset: u32_at(&bytes, 36),
            name_len: u16_at(&bytes, 40),
            flags: u16_at(&bytes, 42),
            reserved: 0,
        });
    }
    tree.strings.resize(header.string_pool_bytes as usize, 0);
    reader.read_exact(&mut tree.strings)?;
    Ok(tree)
}

fn encode(mut writer: impl Write, tree: &MftTree) -> io::Result<()> {
    let volumes = u32::try_from(tree.volumes.len()).map_err(|_| invalid("too many volumes"))?;
    let nodes = u32::try_from(tree.nodes.len()).map_err(|_| invalid("too many nodes"))?;
    let strings =
        u32::try_from(tree.strings.len()).map_err(|_| invalid("string pool exceeds 4 GiB"))?;
    let mut header = [0u8; 48];
    header[..8].copy_from_slice(b"ORBMFT01");
    header[8..12].copy_from_slice(&VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&volumes.to_le_bytes());
    header[16..24].copy_from_slice(&u64::from(nodes).to_le_bytes());
    header[24..32].copy_from_slice(&u64::from(strings).to_le_bytes());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    header[32..40].copy_from_slice(&now.to_le_bytes());
    writer.write_all(&header)?;
    for volume in &tree.volumes {
        let mut bytes = [0u8; 48];
        bytes[0..8].copy_from_slice(&volume.volume_serial.to_le_bytes());
        bytes[8..16].copy_from_slice(&volume.usn_journal_id.to_le_bytes());
        bytes[16..24].copy_from_slice(&volume.last_usn.to_le_bytes());
        bytes[24..32].copy_from_slice(&volume.root_frn.to_le_bytes());
        bytes[32..36].copy_from_slice(&volume.first_node_index.to_le_bytes());
        bytes[36..40].copy_from_slice(&volume.node_count.to_le_bytes());
        bytes[40..42].copy_from_slice(&volume.drive_letter.to_le_bytes());
        bytes[42..44].copy_from_slice(&volume.flags.to_le_bytes());
        writer.write_all(&bytes)?;
    }
    for node in &tree.nodes {
        let mut bytes = [0u8; 48];
        bytes[0..8].copy_from_slice(&node.file_ref_number.to_le_bytes());
        bytes[8..16].copy_from_slice(&node.logical_size.to_le_bytes());
        bytes[16..24].copy_from_slice(&node.allocated_size.to_le_bytes());
        bytes[24..32].copy_from_slice(&node.modified_time_ns100.to_le_bytes());
        bytes[32..36].copy_from_slice(&node.parent_index.to_le_bytes());
        bytes[36..40].copy_from_slice(&node.name_offset.to_le_bytes());
        bytes[40..42].copy_from_slice(&node.name_len.to_le_bytes());
        bytes[42..44].copy_from_slice(&node.flags.to_le_bytes());
        writer.write_all(&bytes)?;
    }
    writer.write_all(&tree.strings)
}

pub fn save(path: &Path, tree: &MftTree) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let mut temporary_name = path.as_os_str().to_os_string();
    temporary_name.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let temporary = std::path::PathBuf::from(temporary_name);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let result = (|| {
        let mut writer = BufWriter::new(file);
        encode(&mut writer, tree)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        // std::fs::rename uses MoveFileExW(REPLACE_EXISTING) on Windows.
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_layout_roundtrip_preserves_frn_and_signed_time() {
        let tree = MftTree {
            volumes: vec![MftVolume {
                volume_serial: u64::MAX,
                last_usn: -9,
                node_count: 1,
                drive_letter: b'F' as u16,
                ..Default::default()
            }],
            nodes: vec![MftNode {
                file_ref_number: 0xfedc_1234_5678_9abc,
                parent_index: u32::MAX,
                name_len: 3,
                modified_time_ns100: i64::MIN,
                allocated_size: 4096,
                ..Default::default()
            }],
            strings: b"abc".to_vec(),
        };
        let mut bytes = Vec::new();
        encode(&mut bytes, &tree).unwrap();
        assert_eq!(bytes.len(), 48 * 3 + 3);
        assert_eq!(&bytes[48 + 16..48 + 24], &(-9i64).to_le_bytes());
        assert_eq!(
            &bytes[96..104],
            &tree.nodes[0].file_ref_number.to_le_bytes()
        );
        assert_eq!(&bytes[40..48], &[0; 8]);
        assert_eq!(&bytes[92..96], &[0; 4]);
        assert_eq!(&bytes[140..144], &[0; 4]);
        let restored = decode(bytes.as_slice(), bytes.len() as u64).unwrap();
        assert_eq!(restored.nodes, tree.nodes);
        assert_eq!(restored.volumes, tree.volumes);
        assert_eq!(restored.strings, tree.strings);
        assert_eq!(std::mem::size_of::<MftNode>(), 48);
        assert_eq!(std::mem::size_of::<MftVolume>(), 48);
    }

    #[test]
    fn rejects_truncation_version_and_overflow_using_header_only() {
        let mut bytes = Vec::new();
        encode(&mut bytes, &MftTree::default()).unwrap();
        assert!(read_header(bytes.as_slice(), 49).is_err());
        for version in [1u32, VERSION + 1] {
            bytes[8..12].copy_from_slice(&version.to_le_bytes());
            assert!(read_header(bytes.as_slice(), 48).is_err());
            assert!(decode(bytes.as_slice(), 48).is_err());
        }
        bytes[8..12].copy_from_slice(&VERSION.to_le_bytes());
        bytes[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(read_header(bytes.as_slice(), 48).is_err());
    }
}
