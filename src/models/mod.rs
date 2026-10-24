use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

pub type NodeId = usize;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScanState {
    #[default]
    Idle,
    Scanning,
    Partial,
    Complete,
    Failed,
}

impl ScanState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "等待选择目录",
            Self::Scanning => "正在扫描 · 当前统计尚未完成",
            Self::Partial => "扫描不完整 · 已保留发现的结果",
            Self::Complete => "扫描完成",
            Self::Failed => "扫描失败",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

#[derive(Debug)]
pub struct Node {
    pub path: Arc<PathBuf>,
    pub parent: Option<NodeId>,
    pub kind: EntryKind,
    pub bytes: u64,
    pub size_overflow: bool,
    pub children: Vec<NodeId>,
    pub pending_directories: usize,
    pub errors: usize,
}

impl Node {
    pub fn is_complete(&self) -> bool {
        self.pending_directories == 0 && self.errors == 0 && !self.size_overflow
    }
}

#[derive(Debug)]
pub struct ScanIssue {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug)]
pub enum ScanEvent {
    Entry {
        path: PathBuf,
        parent: Option<NodeId>,
        kind: EntryKind,
        bytes: u64,
    },
    DirectoryRead(NodeId),
    Issue {
        parent: NodeId,
        issue: ScanIssue,
    },
    Finished(ScanState),
}

#[derive(Default)]
pub struct ScanResults {
    pub state: ScanState,
    pub nodes: Vec<Node>,
    pub issues: Vec<ScanIssue>,
    pub files: usize,
    pub directories: usize,
    pub symlinks: usize,
}

#[derive(Debug)]
pub struct NodeChange {
    pub id: NodeId,
    pub previous_bytes: Option<u64>,
}

#[derive(Default)]
struct Delta {
    bytes: u64,
    overflow: bool,
    opened: usize,
    closed: usize,
    errors: usize,
}

impl Delta {
    fn merge(&mut self, other: &Self) {
        self.overflow |= other.overflow || self.bytes.checked_add(other.bytes).is_none();
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.opened += other.opened;
        self.closed += other.closed;
        self.errors += other.errors;
    }
}

impl ScanResults {
    #[cfg(test)]
    pub fn apply(&mut self, event: ScanEvent) {
        self.apply_batch([event]);
    }

    pub fn apply_batch(&mut self, events: impl IntoIterator<Item = ScanEvent>) -> Vec<NodeChange> {
        let first_new = self.nodes.len();
        let already_overflowed = self.nodes.first().is_some_and(|node| node.size_overflow);
        let mut deltas: BTreeMap<NodeId, Delta> = BTreeMap::new();
        for event in events {
            match event {
                ScanEvent::Entry {
                    path,
                    parent,
                    kind,
                    bytes,
                } => {
                    let id = self.nodes.len();
                    let pending = usize::from(kind == EntryKind::Directory);
                    match kind {
                        EntryKind::File => self.files += 1,
                        EntryKind::Directory => self.directories += 1,
                        EntryKind::Symlink => self.symlinks += 1,
                        EntryKind::Other => {}
                    }
                    self.nodes.push(Node {
                        path: Arc::new(path),
                        parent,
                        kind,
                        bytes,
                        size_overflow: false,
                        children: Vec::new(),
                        pending_directories: pending,
                        errors: 0,
                    });
                    if let Some(parent) = parent {
                        self.nodes[parent].children.push(id);
                        deltas.entry(parent).or_default().merge(&Delta {
                            bytes,
                            opened: pending,
                            ..Default::default()
                        });
                    }
                }
                ScanEvent::DirectoryRead(id) => deltas.entry(id).or_default().closed += 1,
                ScanEvent::Issue { parent, issue } => {
                    self.issues.push(issue);
                    if parent < self.nodes.len() {
                        deltas.entry(parent).or_default().errors += 1;
                    }
                }
                ScanEvent::Finished(state) => self.state = state,
            }
        }
        let mut changes = Vec::new();
        // Parents are always created before their children. Folding backwards
        // visits each affected ancestor only once per batch, not once per file.
        while let Some((id, delta)) = deltas.pop_last() {
            let node = &mut self.nodes[id];
            if id < first_new {
                changes.push(NodeChange {
                    id,
                    previous_bytes: Some(node.bytes),
                });
            }
            node.size_overflow |= delta.overflow || node.bytes.checked_add(delta.bytes).is_none();
            node.bytes = node.bytes.saturating_add(delta.bytes);
            node.pending_directories += delta.opened;
            node.pending_directories -= delta.closed;
            node.errors += delta.errors;
            if let Some(parent) = node.parent {
                deltas.entry(parent).or_default().merge(&delta);
            }
        }
        if let Some(root) = self.nodes.first().filter(|node| node.size_overflow) {
            if !already_overflowed {
                self.issues.push(ScanIssue {
                    path: root.path.as_ref().clone(),
                    message:
                        "逻辑大小总量超出 u64 统计范围，已停止将溢出目录的大小与占比视为精确值。"
                            .into(),
                });
            }
            if self.state == ScanState::Complete {
                self.state = ScanState::Partial;
            }
        }
        changes.extend((first_new..self.nodes.len()).map(|id| NodeChange {
            id,
            previous_bytes: None,
        }));
        changes
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DiskSpace {
    pub capacity: u64,
    pub available: u64,
}

#[cfg(test)]
mod tests;
