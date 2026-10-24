use std::{cmp::Reverse, collections::BTreeSet, sync::Arc};

use crate::models::{NodeChange, NodeId, NodePath, ScanResults, ScanState};

#[cfg(test)]
pub fn sorted_children(results: &ScanResults, directory: NodeId) -> Vec<NodeId> {
    let Some(node) = results.nodes.get(directory) else {
        return Vec::new();
    };
    let mut children = node.children().to_vec();
    sort_nodes(results, &mut children);
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

#[derive(Default)]
pub struct LargeFileIndex {
    files: SizeIndex,
    indexed_nodes: usize,
}

impl LargeFileIndex {
    pub fn update(&mut self, results: &ScanResults) {
        let SizeIndex::Live(files) = &mut self.files else {
            return;
        };
        for (id, node) in results.nodes.iter().enumerate().skip(self.indexed_nodes) {
            if node.kind == crate::models::EntryKind::File && node.hard_link_to().is_none() {
                files.insert(size_key(results, id));
            }
        }
        self.indexed_nodes = results.nodes.len();
        self.files.finish(results.state);
    }

    pub fn write_matching(
        &self,
        results: &ScanResults,
        minimum_bytes: u64,
        output: &mut Vec<NodeId>,
    ) {
        output.clear();
        match &self.files {
            SizeIndex::Live(files) => output.extend(
                files
                    .iter()
                    .take_while(|(size, _, _)| size.0 >= minimum_bytes)
                    .map(|(_, _, id)| *id),
            ),
            SizeIndex::Finished(files) => output.extend(
                files
                    .iter()
                    .copied()
                    .take_while(|id| results.nodes[*id].bytes >= minimum_bytes),
            ),
        }
    }

    #[cfg(test)]
    pub fn matching(&self, results: &ScanResults, minimum_bytes: u64) -> Vec<NodeId> {
        let mut output = Vec::new();
        self.write_matching(results, minimum_bytes, &mut output);
        output
    }
}

type SizeKey = (Reverse<u64>, Arc<NodePath>, NodeId);

enum SizeIndex {
    Live(BTreeSet<SizeKey>),
    Finished(Vec<NodeId>),
}

impl Default for SizeIndex {
    fn default() -> Self {
        Self::Live(BTreeSet::new())
    }
}

impl SizeIndex {
    fn finish(&mut self, state: ScanState) {
        if !is_finished(state) {
            return;
        }
        if let Self::Live(entries) = self {
            let mut ids = Vec::with_capacity(entries.len());
            ids.extend(std::mem::take(entries).into_iter().map(|(_, _, id)| id));
            *self = Self::Finished(ids);
        }
    }

    fn from_ids(results: &ScanResults, ids: impl Iterator<Item = NodeId>) -> Self {
        if is_finished(results.state) {
            let mut ids: Vec<_> = ids.collect();
            sort_nodes(results, &mut ids);
            ids.shrink_to_fit();
            Self::Finished(ids)
        } else {
            Self::Live(ids.map(|id| size_key(results, id)).collect())
        }
    }

    fn write_sorted(&self, output: &mut Vec<NodeId>) {
        output.clear();
        match self {
            Self::Live(entries) => output.extend(entries.iter().map(|(_, _, id)| *id)),
            Self::Finished(entries) => output.extend_from_slice(entries),
        }
    }
}

fn is_finished(state: ScanState) -> bool {
    matches!(
        state,
        ScanState::Complete | ScanState::Partial | ScanState::Failed
    )
}

fn sort_nodes(results: &ScanResults, ids: &mut [NodeId]) {
    ids.sort_unstable_by(|a, b| {
        let left = &results.nodes[*a];
        let right = &results.nodes[*b];
        right
            .bytes
            .cmp(&left.bytes)
            .then_with(|| left.path.cmp(&right.path))
            .then(a.cmp(b))
    });
}

fn size_key(results: &ScanResults, id: NodeId) -> SizeKey {
    let node = &results.nodes[id];
    (Reverse(node.bytes), node.path.clone(), id)
}

#[derive(Default)]
pub struct DirectoryIndex {
    directory: Option<NodeId>,
    entries: SizeIndex,
}

impl DirectoryIndex {
    pub fn select(&mut self, results: &ScanResults, directory: NodeId) {
        if self.directory == Some(directory) {
            return;
        }
        self.directory = Some(directory);
        self.entries = SizeIndex::from_ids(
            results,
            results
                .nodes
                .get(directory)
                .into_iter()
                .flat_map(|node| node.children().iter().copied()),
        );
    }

    pub fn update(&mut self, results: &ScanResults, changes: &[NodeChange]) {
        let Some(directory) = self.directory else {
            return;
        };
        let SizeIndex::Live(entries) = &mut self.entries else {
            return;
        };
        for change in changes {
            let node = &results.nodes[change.id];
            if node.parent() != Some(directory) {
                continue;
            }
            if let Some(previous) = change.previous_bytes {
                if previous == node.bytes {
                    continue;
                }
                entries.remove(&(Reverse(previous), node.path.clone(), change.id));
            }
            entries.insert(size_key(results, change.id));
        }
        self.entries.finish(results.state);
    }

    pub fn write_sorted(&self, output: &mut Vec<NodeId>) {
        self.entries.write_sorted(output);
    }

    #[cfg(test)]
    pub fn sorted(&self) -> Vec<NodeId> {
        let mut output = Vec::new();
        self.write_sorted(&mut output);
        output
    }
}

#[cfg(test)]
mod tests;
