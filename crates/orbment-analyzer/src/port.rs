use crate::process::{self, ProcessIdentity};
use anyhow::{Context, Result, ensure};
use orbment_core::i18n::tr;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    mem::{offset_of, size_of},
    net::{Ipv4Addr, Ipv6Addr},
};
use windows::Win32::{
    Foundation::ERROR_INSUFFICIENT_BUFFER,
    NetworkManagement::IpHelper::{
        GetExtendedTcpTable, GetExtendedUdpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID,
        MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, MIB_UDP6ROW_OWNER_PID,
        MIB_UDP6TABLE_OWNER_PID, MIB_UDPROW_OWNER_PID, MIB_UDPTABLE_OWNER_PID,
        TCP_TABLE_OWNER_PID_ALL, UDP_TABLE_OWNER_PID,
    },
    Networking::WinSock::{AF_INET, AF_INET6},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Protocol {
    Tcp,
    Udp,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum AddressFamily {
    Ipv4,
    Ipv6,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TcpState {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
    DeleteTcb,
    Unknown(u32),
}
impl From<u32> for TcpState {
    fn from(value: u32) -> Self {
        match value {
            1 => Self::Closed,
            2 => Self::Listen,
            3 => Self::SynSent,
            4 => Self::SynReceived,
            5 => Self::Established,
            6 => Self::FinWait1,
            7 => Self::FinWait2,
            8 => Self::CloseWait,
            9 => Self::Closing,
            10 => Self::LastAck,
            11 => Self::TimeWait,
            12 => Self::DeleteTcb,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortEntry {
    pub protocol: Protocol,
    pub address_family: AddressFamily,
    pub local_address: String,
    pub local_port: u16,
    pub remote_address: Option<String>,
    pub remote_port: Option<u16>,
    pub state: Option<TcpState>,
    pub pid: u32,
    pub identity: Option<ProcessIdentity>,
    pub process_name: Option<String>,
    pub path: Option<String>,
    pub command_line: Option<String>,
}

pub fn parse_port_filter(text: &str) -> Result<u16> {
    let text = text.trim();
    let digits = text.strip_prefix(':').unwrap_or(text);
    ensure!(
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
        "{}",
        tr("memory-port-invalid")
    );
    let port = digits.parse::<u16>().context(tr("memory-port-invalid"))?;
    ensure!(port != 0, "{}", tr("memory-port-invalid"));
    Ok(port)
}

pub fn lookup(port: Option<u16>, process_name: Option<&str>) -> Result<Vec<PortEntry>> {
    ensure!(port != Some(0), "{}", tr("memory-port-invalid"));
    let before = process::snapshot_identities()?;
    let mut entries = Vec::new();
    for family in [AddressFamily::Ipv4, AddressFamily::Ipv6] {
        for protocol in [Protocol::Tcp, Protocol::Udp] {
            let buffer = query_table(protocol, family)?;
            match (protocol, family) {
                (Protocol::Tcp, AddressFamily::Ipv4) => {
                    for row in rows::<MIB_TCPROW_OWNER_PID>(
                        &buffer,
                        offset_of!(MIB_TCPTABLE_OWNER_PID, table),
                    )? {
                        entries.push(entry(
                            protocol,
                            family,
                            ipv4(row.dwLocalAddr),
                            row.dwLocalPort,
                            Some(ipv4(row.dwRemoteAddr)),
                            Some(row.dwRemotePort),
                            Some(row.dwState),
                            row.dwOwningPid,
                        ));
                    }
                }
                (Protocol::Tcp, AddressFamily::Ipv6) => {
                    for row in rows::<MIB_TCP6ROW_OWNER_PID>(
                        &buffer,
                        offset_of!(MIB_TCP6TABLE_OWNER_PID, table),
                    )? {
                        entries.push(entry(
                            protocol,
                            family,
                            ipv6(row.ucLocalAddr, row.dwLocalScopeId),
                            row.dwLocalPort,
                            Some(ipv6(row.ucRemoteAddr, row.dwRemoteScopeId)),
                            Some(row.dwRemotePort),
                            Some(row.dwState),
                            row.dwOwningPid,
                        ));
                    }
                }
                (Protocol::Udp, AddressFamily::Ipv4) => {
                    for row in rows::<MIB_UDPROW_OWNER_PID>(
                        &buffer,
                        offset_of!(MIB_UDPTABLE_OWNER_PID, table),
                    )? {
                        entries.push(entry(
                            protocol,
                            family,
                            ipv4(row.dwLocalAddr),
                            row.dwLocalPort,
                            None,
                            None,
                            None,
                            row.dwOwningPid,
                        ));
                    }
                }
                (Protocol::Udp, AddressFamily::Ipv6) => {
                    for row in rows::<MIB_UDP6ROW_OWNER_PID>(
                        &buffer,
                        offset_of!(MIB_UDP6TABLE_OWNER_PID, table),
                    )? {
                        entries.push(entry(
                            protocol,
                            family,
                            ipv6(row.ucLocalAddr, row.dwLocalScopeId),
                            row.dwLocalPort,
                            None,
                            None,
                            None,
                            row.dwOwningPid,
                        ));
                    }
                }
            }
        }
    }
    let after = process::snapshot_identities()?;
    entries.retain(|entry| port.is_none_or(|port| entry.local_port == port));
    let pids: BTreeSet<_> = entries.iter().map(|entry| entry.pid).collect();
    let before = process::by_pid(&before);
    let mut after: BTreeMap<_, _> = after
        .into_iter()
        .filter(|process| pids.contains(&process.pid))
        .map(|process| (process.pid, process))
        .collect();
    for process in after.values_mut() {
        if process.identity.is_some()
            && before
                .get(&process.pid)
                .is_some_and(|first| first.identity == process.identity)
            && process_name
                .is_none_or(|name| process.name.to_lowercase().contains(&name.to_lowercase()))
        {
            if process::enrich_port_process(process).is_err() {
                process.identity = None;
            }
        } else {
            process.identity = None;
        }
    }
    for entry in &mut entries {
        if let (Some(first), Some(last)) = (before.get(&entry.pid), after.get(&entry.pid))
            && first.identity.is_some()
            && first.identity == last.identity
        {
            // Bind table metadata only to a generation alive on both sides of the table read.
            entry.identity = last.identity;
            entry.process_name = Some(last.name.clone());
            entry.path = last.path.clone();
            entry.command_line = last.command_line.clone();
        }
    }
    entries.retain(|entry| matches_filter(entry, port, process_name));
    entries.sort_by(|a, b| {
        (
            a.local_port,
            a.protocol,
            a.address_family,
            a.pid,
            &a.local_address,
        )
            .cmp(&(
                b.local_port,
                b.protocol,
                b.address_family,
                b.pid,
                &b.local_address,
            ))
    });
    Ok(entries)
}

pub fn matches_filter(entry: &PortEntry, port: Option<u16>, process_name: Option<&str>) -> bool {
    port.is_none_or(|port| entry.local_port == port)
        && process_name.is_none_or(|name| {
            entry
                .process_name
                .as_ref()
                .is_some_and(|process| process.to_lowercase().contains(&name.to_lowercase()))
        })
}

fn network_port(raw: u32) -> u16 {
    u16::from_be(raw as u16)
}
fn ipv4(raw: u32) -> String {
    Ipv4Addr::from(raw.to_ne_bytes()).to_string()
}
fn ipv6(raw: [u8; 16], scope: u32) -> String {
    let address = Ipv6Addr::from(raw);
    if scope == 0 {
        address.to_string()
    } else {
        format!("{address}%{scope}")
    }
}

#[allow(clippy::too_many_arguments)]
fn entry(
    protocol: Protocol,
    address_family: AddressFamily,
    local_address: String,
    local_port: u32,
    remote_address: Option<String>,
    remote_port: Option<u32>,
    state: Option<u32>,
    pid: u32,
) -> PortEntry {
    PortEntry {
        protocol,
        address_family,
        local_address,
        local_port: network_port(local_port),
        remote_address,
        remote_port: remote_port.map(network_port),
        state: state.map(TcpState::from),
        pid,
        identity: None,
        process_name: None,
        path: None,
        command_line: None,
    }
}

fn query_table(protocol: Protocol, family: AddressFamily) -> Result<Vec<u8>> {
    let af = u32::from(match family {
        AddressFamily::Ipv4 => AF_INET.0,
        AddressFamily::Ipv6 => AF_INET6.0,
    });
    let mut size = 0;
    let call = |buffer: Option<*mut std::ffi::c_void>, size: &mut u32| unsafe {
        match protocol {
            Protocol::Tcp => {
                GetExtendedTcpTable(buffer, size, false, af, TCP_TABLE_OWNER_PID_ALL, 0)
            }
            Protocol::Udp => GetExtendedUdpTable(buffer, size, false, af, UDP_TABLE_OWNER_PID, 0),
        }
    };
    let result = call(None, &mut size);
    ensure!(
        result == ERROR_INSUFFICIENT_BUFFER.0 || result == 0,
        "{}: {}",
        tr("memory-port-query"),
        result
    );
    loop {
        // DWORD alignment is required by every owner-PID table layout.
        let mut aligned = vec![0u32; (size as usize).div_ceil(size_of::<u32>()).max(1)];
        let result = call(Some(aligned.as_mut_ptr().cast()), &mut size);
        if result == ERROR_INSUFFICIENT_BUFFER.0 {
            continue;
        }
        ensure!(result == 0, "{}: {}", tr("memory-port-query"), result);
        let bytes =
            unsafe { std::slice::from_raw_parts(aligned.as_ptr().cast::<u8>(), size as usize) };
        return Ok(bytes.to_vec());
    }
}

fn rows<T: Copy>(bytes: &[u8], offset: usize) -> Result<Vec<T>> {
    ensure!(
        bytes.len() >= size_of::<u32>(),
        "{}",
        tr("memory-port-query")
    );
    let count = u32::from_ne_bytes(bytes[..4].try_into().expect("four bytes")) as usize;
    ensure!(
        offset <= bytes.len() && count <= (bytes.len() - offset) / size_of::<T>(),
        "{}",
        tr("memory-port-query")
    );
    Ok((0..count)
        .map(|i| unsafe {
            bytes
                .as_ptr()
                .add(offset + i * size_of::<T>())
                .cast::<T>()
                .read_unaligned()
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_byte_order_and_ipv6_scope_are_preserved() {
        assert_eq!(network_port(u32::from(3000u16.to_be())), 3000);
        assert_eq!(ipv4(u32::from_ne_bytes([127, 0, 0, 1])), "127.0.0.1");
        assert_eq!(ipv6(Ipv6Addr::LOCALHOST.octets(), 7), "::1%7");
    }

    #[test]
    fn port_filter_validates_syntax_and_range() {
        assert_eq!(parse_port_filter(" :3000 ").unwrap(), 3000);
        for invalid in [":", "0", "65536", "-1", "::80", "80junk"] {
            assert!(parse_port_filter(invalid).is_err());
        }
    }

    #[test]
    fn process_and_port_filters_intersect_without_hiding_unknown_owners() {
        let mut row = entry(
            Protocol::Udp,
            AddressFamily::Ipv6,
            "::".into(),
            u32::from(8080u16.to_be()),
            None,
            None,
            None,
            42,
        );
        assert!(matches_filter(&row, Some(8080), None));
        assert!(!matches_filter(&row, None, Some("node")));
        row.process_name = Some("Node.EXE".into());
        assert!(matches_filter(&row, Some(8080), Some("node")));
        assert!(!matches_filter(&row, Some(3000), Some("node")));
    }

    #[test]
    fn truncated_native_table_is_rejected() {
        let bytes = 2u32.to_ne_bytes();
        assert!(rows::<MIB_UDPROW_OWNER_PID>(&bytes, 4).is_err());
    }
}
