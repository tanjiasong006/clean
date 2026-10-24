use std::{
    cmp::Ordering,
    collections::BTreeMap,
    ffi::OsStr,
    fmt,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
};

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
pub struct NodePath {
    parent: Option<Arc<NodePath>>,
    name: Box<OsStr>,
}

impl NodePath {
    pub fn file_name(&self) -> Option<&OsStr> {
        Path::new(&self.name).file_name()
    }

    pub fn name(&self) -> &OsStr {
        &self.name
    }

    pub fn to_path_buf(&self) -> PathBuf {
        let mut components = Vec::new();
        let mut current = self;
        while let Some(parent) = &current.parent {
            components.push(current.name());
            current = parent;
        }
        let mut path = PathBuf::from(current.name());
        for name in components.into_iter().rev() {
            path.push(name);
        }
        path
    }

    pub fn display(&self) -> &Self {
        self
    }

    fn depth(&self) -> usize {
        let mut depth = 0;
        let mut current = self;
        while let Some(parent) = &current.parent {
            depth += 1;
            current = parent;
        }
        depth
    }
}

impl fmt::Display for NodePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_path_buf().display().fmt(f)
    }
}

impl Ord for NodePath {
    fn cmp(&self, other: &Self) -> Ordering {
        if std::ptr::eq(self, other) {
            return Ordering::Equal;
        }
        if let (Some(a), Some(b)) = (&self.parent, &other.parent)
            && Arc::ptr_eq(a, b)
        {
            return self.name.cmp(&other.name);
        }
        let self_depth = self.depth();
        let other_depth = other.depth();
        let mut a = self;
        let mut b = other;
        for _ in other_depth..self_depth {
            a = a.parent.as_deref().unwrap();
        }
        for _ in self_depth..other_depth {
            b = b.parent.as_deref().unwrap();
        }
        if std::ptr::eq(a, b) {
            return self_depth.cmp(&other_depth);
        }
        // Shared ancestry lets ties compare components without allocating absolute paths.
        while let (Some(a_parent), Some(b_parent)) = (&a.parent, &b.parent) {
            if Arc::ptr_eq(a_parent, b_parent) {
                let order = a.name.cmp(&b.name);
                if order != Ordering::Equal {
                    return order;
                }
                break;
            }
            a = a_parent;
            b = b_parent;
        }
        self.to_path_buf().cmp(&other.to_path_buf())
    }
}

impl PartialOrd for NodePath {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for NodePath {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for NodePath {}

#[derive(Debug)]
pub struct Node {
    pub path: Arc<NodePath>,
    pub kind: EntryKind,
    pub bytes: u64,
    pub size_overflow: bool,
    parent: Option<NonZeroUsize>,
    hard_link_to: Option<NonZeroUsize>,
    directory: Option<Box<DirectoryData>>,
}

#[derive(Debug)]
struct DirectoryData {
    children: Vec<NodeId>,
    pending: usize,
    errors: usize,
}

impl Node {
    pub fn parent(&self) -> Option<NodeId> {
        self.parent.map(|id| id.get() - 1)
    }

    pub fn hard_link_to(&self) -> Option<NodeId> {
        self.hard_link_to.map(|id| id.get() - 1)
    }

    pub fn children(&self) -> &[NodeId] {
        self.directory.as_ref().map_or(&[], |data| &data.children)
    }

    pub fn pending_directories(&self) -> usize {
        self.directory.as_ref().map_or(0, |data| data.pending)
    }

    pub fn errors(&self) -> usize {
        self.directory.as_ref().map_or(0, |data| data.errors)
    }

    pub fn is_complete(&self) -> bool {
        self.pending_directories() == 0 && self.errors() == 0 && !self.size_overflow
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
        hard_link_to: Option<NodeId>,
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
    pub hard_links: usize,
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
                    hard_link_to,
                } => {
                    let id = self.nodes.len();
                    let pending = usize::from(kind == EntryKind::Directory);
                    self.hard_links += usize::from(hard_link_to.is_some());
                    match kind {
                        EntryKind::File => self.files += 1,
                        EntryKind::Directory => self.directories += 1,
                        EntryKind::Symlink => self.symlinks += 1,
                        EntryKind::Other => {}
                    }
                    let path = Arc::new(NodePath {
                        parent: parent.map(|id| self.nodes[id].path.clone()),
                        name: if parent.is_some() {
                            path.file_name().unwrap().into()
                        } else {
                            path.into_os_string().into_boxed_os_str()
                        },
                    });
                    self.nodes.push(Node {
                        path,
                        parent: parent.map(|id| NonZeroUsize::new(id + 1).unwrap()),
                        kind,
                        bytes,
                        size_overflow: false,
                        hard_link_to: hard_link_to.map(|id| NonZeroUsize::new(id + 1).unwrap()),
                        directory: (kind == EntryKind::Directory).then(|| {
                            Box::new(DirectoryData {
                                children: Vec::new(),
                                pending,
                                errors: 0,
                            })
                        }),
                    });
                    if let Some(parent) = parent {
                        self.nodes[parent]
                            .directory
                            .as_mut()
                            .unwrap()
                            .children
                            .push(id);
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
            let directory = node.directory.as_mut().unwrap();
            directory.pending += delta.opened;
            directory.pending -= delta.closed;
            directory.errors += delta.errors;
            if let Some(parent) = node.parent() {
                deltas.entry(parent).or_default().merge(&delta);
            }
        }
        if let Some(root) = self.nodes.first().filter(|node| node.size_overflow) {
            if !already_overflowed {
                self.issues.push(ScanIssue {
                    path: root.path.to_path_buf(),
                    message:
                        "已分配空间总量超出 u64 统计范围，已停止将溢出目录的大小与占比视为精确值。"
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
