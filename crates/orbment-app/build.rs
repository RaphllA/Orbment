use std::io::{self, Error, ErrorKind};

#[cfg(not(test))]
fn main() {
    println!("cargo:rerun-if-changed=orbment.manifest");
    println!("cargo:rerun-if-changed=../../assets/icons/orbment.ico");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let manifest_directory = std::path::PathBuf::from(
            std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"),
        );
        let icon = std::fs::read(manifest_directory.join("../../assets/icons/orbment.ico"))
            .expect("read assets/icons/orbment.ico");
        let resources = icon_resources(&icon).expect("compile assets/icons/orbment.ico resources");
        let resource_path =
            std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("build output directory"))
                .join("orbment_icon.res");
        std::fs::write(&resource_path, resources).expect("write orbment_icon.res");
        println!(
            "cargo:rustc-link-arg-bin=orbment={}",
            resource_path.display()
        );

        let manifest = manifest_directory.join("orbment.manifest");
        println!("cargo:rustc-link-arg-bin=orbment=/MANIFEST:EMBED");
        println!(
            "cargo:rustc-link-arg-bin=orbment=/MANIFESTINPUT:{}",
            manifest.display()
        );
        println!("cargo:rustc-link-arg-bin=orbment=/MANIFESTUAC:NO");
        // MCP/CLI never call graphics entrypoints, so keep these DLLs off their loader path.
        println!("cargo:rustc-link-arg-bin=orbment=delayimp.lib");
        for dll in ["d3d11.dll", "d2d1.dll", "dwrite.dll", "d3dcompiler_47.dll"] {
            println!("cargo:rustc-link-arg-bin=orbment=/DELAYLOAD:{dll}");
        }
    }
}

pub(crate) fn icon_resources(ico: &[u8]) -> io::Result<Vec<u8>> {
    if ico.len() < 6 || ico[..4] != [0, 0, 1, 0] {
        return Err(Error::new(ErrorKind::InvalidData, "invalid ICO header"));
    }
    let count = u16::from_le_bytes([ico[4], ico[5]]);
    let directory_end = 6 + usize::from(count) * 16;
    if count == 0 || ico.len() < directory_end {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "empty or truncated ICO directory",
        ));
    }

    let mut resources = Vec::new();
    write_resource(&mut resources, 0, 0, 0, &[]);
    let mut group = Vec::with_capacity(6 + usize::from(count) * 14);
    group.extend_from_slice(&ico[..6]);
    for (index, entry) in ico[6..directory_end].as_chunks::<16>().0.iter().enumerate() {
        let id = index as u16 + 1;
        let size = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
        let offset = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
        if entry[3] != 0 || size == 0 || offset < directory_end {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("invalid ICO frame {id} directory entry"),
            ));
        }
        let frame = offset
            .checked_add(size)
            .and_then(|end| ico.get(offset..end))
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidData,
                    format!("ICO frame {id} extends beyond the input"),
                )
            })?;
        write_resource(&mut resources, 3, id, 0x1030, frame);
        // GRPICONDIRENTRY replaces the ICO file offset with an RT_ICON resource ID.
        group.extend_from_slice(&entry[..12]);
        group.extend_from_slice(&id.to_le_bytes());
    }
    write_resource(&mut resources, 14, 1, 0x1030, &group);
    Ok(resources)
}

fn write_resource(
    output: &mut Vec<u8>,
    resource_type: u16,
    id: u16,
    memory_flags: u16,
    data: &[u8],
) {
    output.extend_from_slice(&(data.len() as u32).to_le_bytes());
    output.extend_from_slice(&32_u32.to_le_bytes());
    for ordinal in [0xffff, resource_type, 0xffff, id] {
        output.extend_from_slice(&ordinal.to_le_bytes());
    }
    output.extend_from_slice(&0_u32.to_le_bytes()); // DataVersion
    output.extend_from_slice(&memory_flags.to_le_bytes());
    output.extend_from_slice(&0_u16.to_le_bytes()); // Neutral language
    output.extend_from_slice(&0_u32.to_le_bytes()); // Version
    output.extend_from_slice(&0_u32.to_le_bytes()); // Characteristics
    output.extend_from_slice(data);
    output.resize(output.len().next_multiple_of(4), 0);
}
