use crate::{
    memory::{MemorySnapshot, VirtualKind, VirtualMemorySnapshot},
    treemap::Node,
};
use orbment_core::i18n::tr;
use std::collections::BTreeMap;

pub fn process_node_id(pid: u32) -> String {
    format!("process:{pid}")
}

pub fn memory_tree(snapshot: &MemorySnapshot) -> Node {
    let families = snapshot
        .families
        .iter()
        .map(|family| {
            let children: Vec<Node> = family
                .processes
                .iter()
                .map(|p| Node {
                    id: process_node_id(p.pid),
                    label: format!("{} ({})", p.name, p.pid),
                    bytes: p.working_set,
                    children: Vec::new(),
                })
                .collect();
            Node {
                id: format!("family:{}", children.first().map_or("", |n| n.id.as_str())),
                label: family.name.clone(),
                bytes: family.working_set,
                children,
            }
        })
        .collect();
    let (free, cache) = snapshot.memory_lists.as_ref().map_or(
        (snapshot.physical_available, snapshot.system_cache),
        |lists| {
            (
                lists.free_bytes,
                lists.standby_bytes.saturating_add(lists.modified_bytes),
            )
        },
    );
    // The physical chart is a bounded approximation. Raw process counters remain untouched
    // because shared working-set pages make an exact additive physical partition impossible.
    let mut remaining = snapshot.physical_total;
    let free = free.min(remaining);
    remaining -= free;
    let pools = (snapshot.kernel_paged + snapshot.kernel_nonpaged).min(remaining);
    remaining -= pools;
    let cache = cache.min(remaining);
    remaining -= cache;
    let working_set = snapshot.process_working_set.min(remaining);
    remaining -= working_set;
    Node {
        id: "memory".into(),
        label: tr("memory-tree-approximate"),
        bytes: snapshot.physical_total,
        children: vec![
            Node {
                id: "processes".into(),
                label: tr("memory-tree-processes"),
                bytes: working_set,
                children: families,
            },
            Node {
                id: "cache".into(),
                label: tr("memory-tree-cache"),
                bytes: cache,
                children: Vec::new(),
            },
            Node {
                id: "pools".into(),
                label: tr("memory-tree-pools"),
                bytes: pools,
                children: Vec::new(),
            },
            Node {
                id: "free".into(),
                label: tr("memory-tree-free"),
                bytes: free,
                children: Vec::new(),
            },
            Node {
                id: "unaccounted".into(),
                label: tr("memory-tree-other"),
                bytes: remaining,
                children: Vec::new(),
            },
        ],
    }
}

pub fn virtual_tree(snapshot: &VirtualMemorySnapshot) -> Node {
    let mut kinds: BTreeMap<VirtualKind, BTreeMap<String, Vec<Node>>> = BTreeMap::new();
    for region in &snapshot.regions {
        let allocation = region
            .mapped_path
            .clone()
            .unwrap_or_else(|| format!("0x{:X}", region.allocation_base));
        kinds
            .entry(region.kind)
            .or_default()
            .entry(allocation)
            .or_default()
            .push(Node {
                id: format!("region:{:X}", region.base_address),
                label: format!("0x{:X}", region.base_address),
                bytes: region.bytes,
                children: Vec::new(),
            });
    }
    let children = kinds
        .into_iter()
        .map(|(kind, allocations)| {
            let children: Vec<Node> = allocations
                .into_iter()
                .map(|(name, regions)| Node {
                    id: format!("allocation:{kind:?}:{name}"),
                    label: name,
                    bytes: regions.iter().map(|n| n.bytes).sum(),
                    children: regions,
                })
                .collect();
            Node {
                id: format!("virtual:{kind:?}"),
                label: tr(match kind {
                    VirtualKind::Image => "memory-tree-image",
                    VirtualKind::MappedFile => "memory-tree-mapped",
                    VirtualKind::PrivateCommit => "memory-tree-private",
                }),
                bytes: children.iter().map(|n| n.bytes).sum(),
                children,
            }
        })
        .collect();
    Node {
        id: process_node_id(snapshot.identity.pid),
        label: format!("{} {}", tr("memory-tree-virtual"), snapshot.identity.pid),
        bytes: snapshot.committed_bytes,
        children,
    }
}
