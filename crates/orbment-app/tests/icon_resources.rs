#[path = "../build.rs"]
mod build_script;

use std::io::ErrorKind;

const ICO: &[u8] = include_bytes!("../../../assets/icons/orbment.ico");

#[derive(Debug)]
struct Resource<'a> {
    resource_type: u16,
    id: u16,
    memory_flags: u16,
    data: &'a [u8],
}

fn word(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn dword(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_resources(bytes: &[u8]) -> Vec<Resource<'_>> {
    let mut resources = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        assert_eq!(cursor % 4, 0, "resource headers must be DWORD-aligned");
        let header = &bytes[cursor..cursor + 32];
        let size = dword(header, 0) as usize;
        assert_eq!(dword(header, 4), 32);
        assert_eq!(word(header, 8), 0xffff, "type must be an ordinal");
        assert_eq!(word(header, 12), 0xffff, "name must be an ordinal");
        assert_eq!(dword(header, 16), 0, "DataVersion");
        assert_eq!(word(header, 22), 0, "neutral language");
        assert_eq!(dword(header, 24), 0, "Version");
        assert_eq!(dword(header, 28), 0, "Characteristics");
        let start = cursor + 32;
        let end = start + size;
        resources.push(Resource {
            resource_type: word(header, 10),
            id: word(header, 14),
            memory_flags: word(header, 20),
            data: &bytes[start..end],
        });
        cursor = (end + 3) & !3;
        assert!(bytes[end..cursor].iter().all(|&byte| byte == 0));
    }
    assert_eq!(cursor, bytes.len());
    resources
}

#[test]
fn embedded_icon_resources_preserve_all_png_frames_and_group_references() {
    let bytes = build_script::icon_resources(ICO).unwrap();
    let resources = read_resources(&bytes);
    assert_eq!(resources.len(), 11);
    let empty = &resources[0];
    assert_eq!(empty.resource_type, 0);
    assert_eq!(empty.id, 0);
    assert_eq!(empty.memory_flags, 0);
    assert!(empty.data.is_empty());

    let group = &resources[10];
    assert_eq!(group.resource_type, 14);
    assert_eq!(group.id, 1);
    assert_eq!(group.memory_flags, 0x1030);
    assert_eq!(&group.data[..6], &[0, 0, 1, 0, 9, 0]);
    assert_eq!(group.data.len(), 6 + 9 * 14);

    for (index, dimension) in [16, 20, 24, 32, 40, 48, 64, 128, 256]
        .into_iter()
        .enumerate()
    {
        let source_entry = &ICO[6 + index * 16..6 + (index + 1) * 16];
        let group_entry = &group.data[6 + index * 14..6 + (index + 1) * 14];
        assert_eq!(&group_entry[..12], &source_entry[..12]);
        assert_eq!(word(group_entry, 12), index as u16 + 1);
        for &size in &group_entry[..2] {
            assert_eq!(if size == 0 { 256 } else { u16::from(size) }, dimension);
        }
        assert_eq!(word(group_entry, 4), 1);
        assert_eq!(word(group_entry, 6), 32);

        let frame = &resources[index + 1];
        assert_eq!(frame.resource_type, 3);
        assert_eq!(frame.id, word(group_entry, 12));
        assert_eq!(frame.memory_flags, 0x1030);
        let size = dword(source_entry, 8) as usize;
        let offset = dword(source_entry, 12) as usize;
        assert_eq!(frame.data.len(), size);
        assert_eq!(frame.data, &ICO[offset..offset + size]);
        assert_eq!(&frame.data[..8], b"\x89PNG\r\n\x1a\n");
    }
}

fn assert_invalid(ico: &[u8]) {
    let error = build_script::icon_resources(ico).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidData);
}

#[test]
fn rejects_invalid_headers_and_truncated_directories() {
    for length in 0..6 {
        assert_invalid(&ICO[..length]);
    }
    let directory_end = 6 + usize::from(word(ICO, 4)) * 16;
    assert_invalid(&ICO[..6]);
    assert_invalid(&ICO[..directory_end - 1]);
    for (offset, value) in [(0, 1_u16), (2, 0), (2, 2), (4, 0), (4, u16::MAX)] {
        let mut malformed = ICO.to_vec();
        malformed[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        assert_invalid(&malformed);
    }
}

#[test]
fn rejects_invalid_frame_entries_and_truncated_payloads() {
    let mut reserved = ICO.to_vec();
    reserved[6 + 3] = 1;
    assert_invalid(&reserved);

    let directory_end = 6 + u32::from(word(ICO, 4)) * 16;
    for (offset, value) in [
        (6 + 8, 0),
        (6 + 8, u32::MAX),
        (6 + 12, 0),
        (6 + 12, directory_end - 1),
        (6 + 12, u32::MAX),
        (6 + 12, ICO.len() as u32),
    ] {
        let mut malformed = ICO.to_vec();
        malformed[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        assert_invalid(&malformed);
    }
    assert_invalid(&ICO[..ICO.len() - 1]);
}
