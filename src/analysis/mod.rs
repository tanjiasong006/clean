use std::{cmp::Reverse, collections::BTreeSet, path::PathBuf, sync::Arc};

use crate::models::{NodeChange, NodeId, ScanResults};

#[cfg(test)]
pub fn sorted_children(results: &ScanResults, directory: NodeId) -> Vec<NodeId> {
    let Some(node) = results.nodes.get(directory) else {
        return Vec::new();
    };
    let mut children = node.children.clone();
    children.sort_unstable_by(|a, b| {
        let a = &results.nodes[*a];
        let b = &results.nodes[*b];
        b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path))
    });
    children
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// Scan nodes are append-only and file sizes are fixed at observation time.
// Index new files once instead of sorting the full scan on every UI tick.
#[derive(Default)]
pub struct LargeFileIndex {
    files: BTreeSet<SizeKey>,
    indexed_nodes: usize,
}

impl LargeFileIndex {
    pub fn update(&mut self, results: &ScanResults) {
        for (id, node) in results.nodes.iter().enumerate().skip(self.indexed_nodes) {
            if node.kind == crate::models::EntryKind::File {
                self.files.insert(size_key(results, id));
            }
        }
        self.indexed_nodes = results.nodes.len();
    }

    pub fn matching(&self, minimum_bytes: u64) -> Vec<NodeId> {
        self.files
            .iter()
            .take_while(|(size, _, _)| size.0 >= minimum_bytes)
            .map(|(_, _, id)| *id)
            .collect()
    }
}

type SizeKey = (Reverse<u64>, Arc<PathBuf>, NodeId);

fn size_key(results: &ScanResults, id: NodeId) -> SizeKey {
    let node = &results.nodes[id];
    (Reverse(node.bytes), node.path.clone(), id)
}

#[derive(Default)]
pub struct DirectoryIndex {
    directory: Option<NodeId>,
    entries: BTreeSet<SizeKey>,
}

impl DirectoryIndex {
    pub fn select(&mut self, results: &ScanResults, directory: NodeId) {
        if self.directory == Some(directory) {
            return;
        }
        self.directory = Some(directory);
        self.entries = results
            .nodes
            .get(directory)
            .into_iter()
            .flat_map(|node| &node.children)
            .map(|id| size_key(results, *id))
            .collect();
    }

    pub fn update(&mut self, results: &ScanResults, changes: &[NodeChange]) {
        let Some(directory) = self.directory else {
            return;
        };
        for change in changes {
            let node = &results.nodes[change.id];
            if node.parent != Some(directory) {
                continue;
            }
            if let Some(previous) = change.previous_bytes {
                if previous == node.bytes {
                    continue;
                }
                self.entries
                    .remove(&(Reverse(previous), node.path.clone(), change.id));
            }
            self.entries.insert(size_key(results, change.id));
        }
    }

    pub fn sorted(&self) -> Vec<NodeId> {
        self.entries.iter().map(|(_, _, id)| *id).collect()
    }
}

#[cfg(test)]
mod tests;
