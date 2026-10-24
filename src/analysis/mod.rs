use std::cmp::Ordering;

use crate::models::{EntryKind, NodeChange, NodeId, ScanResults};

/// Files at or above this size stay in the incremental large-file index. Smaller
/// files are found by scanning node storage on demand, so the index cannot grow
/// with the number of ordinary files.
pub const LARGE_FILE_FLOOR: u64 = 100 * 1024 * 1024;

const INDEX_CHUNK: usize = 1 << 16;

fn push_id(ids: &mut Vec<u32>, id: NodeId) {
    if ids.len() == ids.capacity() {
        ids.reserve_exact(INDEX_CHUNK);
    }
    ids.push(id as u32);
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

/// Sizes are compared as 64-bit so escaped values stay exact, then names give
/// path order.
fn compare_siblings(results: &ScanResults, left: NodeId, right: NodeId) -> Ordering {
    results
        .bytes(right)
        .cmp(&results.bytes(left))
        .then_with(|| results.name_bytes(left).cmp(&results.name_bytes(right)))
        .then_with(|| left.cmp(&right))
}

fn compare_paths(results: &ScanResults, left: NodeId, right: NodeId) -> Ordering {
    results
        .bytes(right)
        .cmp(&results.bytes(left))
        .then_with(|| results.cmp_path(left, right))
        .then_with(|| left.cmp(&right))
}

#[cfg(test)]
pub fn sorted_children(results: &ScanResults, directory: NodeId) -> Vec<NodeId> {
    let mut children: Vec<NodeId> = results.child_ids(directory).collect();
    children.sort_unstable_by(|left, right| compare_siblings(results, *left, *right));
    children
}

#[derive(Default)]
pub struct LargeFileIndex {
    indexed_nodes: usize,
    entries: Vec<u32>,
    sorted: bool,
    all: Option<Vec<u32>>,
}

impl LargeFileIndex {
    pub fn update(&mut self, results: &ScanResults) {
        if self.indexed_nodes > results.len() {
            self.reset();
            return;
        }
        for id in self.indexed_nodes..results.len() {
            if results.kind(id) == EntryKind::File
                && results.hard_link_to(id).is_none()
                && results.bytes(id) >= LARGE_FILE_FLOOR
            {
                push_id(&mut self.entries, id);
                self.sorted = false;
            }
        }
        if self.indexed_nodes != results.len() {
            self.indexed_nodes = results.len();
            self.all = None;
        }
    }

    fn reset(&mut self) {
        self.indexed_nodes = 0;
        self.entries.clear();
        self.sorted = true;
        self.all = None;
    }

    /// Prepares the sorted list for a threshold. No filtered copy is kept:
    /// files at or above the threshold are a prefix of the size-descending list.
    pub fn sync(&mut self, results: &ScanResults, minimum_bytes: u64) {
        if minimum_bytes >= LARGE_FILE_FLOOR {
            if !self.sorted {
                self.entries.sort_unstable_by(|left, right| {
                    compare_paths(results, *left as usize, *right as usize)
                });
                self.sorted = true;
            }
            return;
        }
        if self.all.is_none() {
            let mut ids = Vec::new();
            for id in 0..results.len() {
                if results.kind(id) == EntryKind::File && results.hard_link_to(id).is_none() {
                    push_id(&mut ids, id);
                }
            }
            ids.sort_unstable_by(|left, right| {
                compare_paths(results, *left as usize, *right as usize)
            });
            self.all = Some(ids);
        }
    }

    fn list(&self, minimum_bytes: u64) -> &[u32] {
        if minimum_bytes >= LARGE_FILE_FLOOR {
            &self.entries
        } else {
            self.all.as_deref().unwrap_or(&[])
        }
    }

    pub fn visible_len(&self, results: &ScanResults, minimum_bytes: u64) -> usize {
        self.list(minimum_bytes)
            .partition_point(|id| results.bytes(*id as usize) >= minimum_bytes)
    }

    pub fn visible_id(&self, minimum_bytes: u64, index: usize) -> NodeId {
        self.list(minimum_bytes)[index] as usize
    }

    #[cfg(test)]
    pub fn write_matching(
        &mut self,
        results: &ScanResults,
        minimum_bytes: u64,
        output: &mut Vec<u32>,
    ) {
        self.sync(results, minimum_bytes);
        let length = self.visible_len(results, minimum_bytes);
        output.clear();
        output.extend_from_slice(&self.list(minimum_bytes)[..length]);
    }

    #[cfg(test)]
    pub fn matching(&mut self, results: &ScanResults, minimum_bytes: u64) -> Vec<NodeId> {
        let mut output = Vec::new();
        self.write_matching(results, minimum_bytes, &mut output);
        output.into_iter().map(|id| id as usize).collect()
    }
}

/// Sorted children of the directory being viewed. New children are appended and
/// merged; size changes in existing children trigger a full re-sort. Memory is
/// four bytes per child, independent of the rest of the scan.
#[derive(Default)]
pub struct DirectoryIndex {
    directory: Option<NodeId>,
    entries: Vec<u32>,
    pending: Vec<u32>,
    dirty: bool,
}

impl DirectoryIndex {
    pub fn select(&mut self, results: &ScanResults, directory: NodeId) {
        if self.directory == Some(directory) {
            return;
        }
        self.directory = Some(directory);
        self.entries.clear();
        for id in results.child_ids(directory) {
            push_id(&mut self.entries, id);
        }
        self.pending.clear();
        self.entries.sort_unstable_by(|left, right| {
            compare_siblings(results, *left as usize, *right as usize)
        });
        self.dirty = false;
    }

    pub fn update(&mut self, results: &ScanResults, changes: &[NodeChange]) {
        let Some(directory) = self.directory else {
            return;
        };
        let range = results.child_ids(directory);
        for change in changes {
            if !range.contains(&change.id) {
                continue;
            }
            match change.previous_bytes {
                None => push_id(&mut self.pending, change.id),
                Some(previous) if previous != results.bytes(change.id) => self.dirty = true,
                Some(_) => {}
            }
        }
    }

    pub fn sync(&mut self, results: &ScanResults) {
        self.merge(results);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn id(&self, index: usize) -> NodeId {
        self.entries[index] as usize
    }

    #[cfg(test)]
    pub fn write_sorted(&mut self, results: &ScanResults, output: &mut Vec<u32>) {
        self.sync(results);
        output.clear();
        output.extend_from_slice(&self.entries);
    }

    fn merge(&mut self, results: &ScanResults) {
        if self.dirty {
            self.entries.append(&mut self.pending);
            self.entries.sort_unstable_by(|left, right| {
                compare_siblings(results, *left as usize, *right as usize)
            });
            self.dirty = false;
            return;
        }
        if self.pending.is_empty() {
            return;
        }
        self.pending.sort_unstable_by(|left, right| {
            compare_siblings(results, *left as usize, *right as usize)
        });
        let old = self.entries.len();
        let added = self.pending.len();
        self.entries.reserve_exact(added);
        self.entries.resize(old + added, 0);
        let mut write = old + added;
        let mut read = old;
        let mut next = added;
        while next > 0 {
            // Place the entry that sorts later at the back.
            let take_entry = read > 0
                && compare_siblings(
                    results,
                    self.entries[read - 1] as usize,
                    self.pending[next - 1] as usize,
                ) == Ordering::Greater;
            write -= 1;
            if take_entry {
                self.entries[write] = self.entries[read - 1];
                read -= 1;
            } else {
                self.entries[write] = self.pending[next - 1];
                next -= 1;
            }
        }
        self.pending.clear();
    }

    #[cfg(test)]
    pub fn sorted(&mut self, results: &ScanResults) -> Vec<NodeId> {
        let mut output = Vec::new();
        self.write_sorted(results, &mut output);
        output.into_iter().map(|id| id as usize).collect()
    }
}
