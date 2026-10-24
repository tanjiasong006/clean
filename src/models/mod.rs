//! Compact scan results. A node is eight bytes: a name offset and a packed
//! kind/payload word. Names live in chunked arenas; directory metadata lives in
//! a separate table. Entries of one directory must be emitted contiguously so
//! that a child's parent is found from the directory's child range.

use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{BTreeMap, HashMap},
    ffi::{OsStr, OsString},
    fmt,
    ops::Range,
    path::PathBuf,
};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

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

const KIND_FILE: u32 = 0 << 30;
const KIND_DIRECTORY: u32 = 1 << 30;
const KIND_SYMLINK: u32 = 2 << 30;
const KIND_OTHER: u32 = 3 << 30;
const KIND_MASK: u32 = 3 << 30;
/// A payload too large for the inline field stores its value in a side map.
const ESCAPED: u32 = 1 << 29;
const PAYLOAD_MASK: u32 = (1 << 29) - 1;

const BLOCK_SIZE: u64 = 512;
const MAX_INLINE_BLOCKS: u64 = PAYLOAD_MASK as u64;
const MAX_DIRECTORIES: usize = PAYLOAD_MASK as usize;

const NO_PREFIX: u8 = u8::MAX;
const NAME_CHUNK: usize = 1 << 20;
const NODE_CHUNK: usize = 1 << 16;
const INDEX_CHUNK: usize = 1 << 16;

#[derive(Clone, Copy, Debug)]
struct Node {
    name: u32,
    info: u32,
}

impl Node {
    fn kind(self) -> EntryKind {
        match self.info & KIND_MASK {
            KIND_DIRECTORY => EntryKind::Directory,
            KIND_SYMLINK => EntryKind::Symlink,
            KIND_OTHER => EntryKind::Other,
            _ => EntryKind::File,
        }
    }

    fn ordinal(self) -> usize {
        (self.info & PAYLOAD_MASK) as usize
    }

    fn escaped(self) -> bool {
        self.info & ESCAPED != 0
    }
}

/// Names are packed back to back in fixed-size chunks, in node order. A name's
/// end is the next node's name offset (or the arena end for the last node), so
/// no per-name terminator is stored. Names may span chunks.
#[derive(Default)]
struct NameArena {
    chunks: Vec<Vec<u8>>,
}

impl NameArena {
    fn push(&mut self, name: &[u8]) -> u32 {
        let offset = self.len();
        let end = offset + name.len();
        assert!(
            end <= u32::MAX as usize,
            "scan contains too many bytes of file names for compact offsets"
        );
        while self.chunks.len() * NAME_CHUNK < end {
            self.chunks.push(Vec::with_capacity(NAME_CHUNK));
        }
        self.write(offset, name);
        offset as u32
    }

    fn len(&self) -> usize {
        match self.chunks.last() {
            Some(last) => (self.chunks.len() - 1) * NAME_CHUNK + last.len(),
            None => 0,
        }
    }

    fn read(&self, offset: usize, len: usize) -> Cow<'_, [u8]> {
        if len == 0 {
            return Cow::Borrowed(&[][..]);
        }
        let index = offset / NAME_CHUNK;
        let position = offset % NAME_CHUNK;
        let chunk = &self.chunks[index];
        if position + len <= chunk.len() {
            return Cow::Borrowed(&chunk[position..position + len]);
        }
        let mut bytes = vec![0; len];
        self.read_into(offset, &mut bytes);
        Cow::Owned(bytes)
    }

    fn read_into(&self, offset: usize, output: &mut [u8]) {
        let mut index = offset / NAME_CHUNK;
        let mut position = offset % NAME_CHUNK;
        let mut written = 0;
        while written < output.len() {
            let available = &self.chunks[index][position..];
            let take = available.len().min(output.len() - written);
            output[written..written + take].copy_from_slice(&available[..take]);
            written += take;
            index += 1;
            position = 0;
        }
    }

    fn write(&mut self, offset: usize, bytes: &[u8]) {
        let mut index = offset / NAME_CHUNK;
        let mut position = offset % NAME_CHUNK;
        let mut written = 0;
        while written < bytes.len() {
            let chunk = &mut self.chunks[index];
            if chunk.len() < position {
                chunk.resize(position, 0);
            }
            let available = NAME_CHUNK - position;
            let take = available.min(bytes.len() - written);
            if chunk.len() < position + take {
                chunk.resize(position + take, 0);
            }
            chunk[position..position + take].copy_from_slice(&bytes[written..written + take]);
            written += take;
            index += 1;
            position = 0;
        }
    }

    fn truncate(&mut self, end: usize) {
        let index = end / NAME_CHUNK;
        let within = end % NAME_CHUNK;
        if index >= self.chunks.len() {
            return;
        }
        if within == 0 {
            self.chunks.truncate(index);
        } else {
            self.chunks[index].truncate(within);
            self.chunks.truncate(index + 1);
        }
    }
}

#[cfg(not(unix))]
fn os_bytes(name: &OsStr) -> Cow<'_, [u8]> {
    match name.to_str() {
        Some(text) => Cow::Borrowed(text.as_bytes()),
        None => Cow::Owned(name.to_string_lossy().into_owned().into_bytes()),
    }
}

#[cfg(unix)]
fn os_bytes(name: &OsStr) -> Cow<'_, [u8]> {
    Cow::Borrowed(name.as_bytes())
}

#[cfg(not(unix))]
fn os_string_from_bytes(bytes: Vec<u8>) -> OsString {
    OsString::from(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(unix)]
fn os_string_from_bytes(bytes: Vec<u8>) -> OsString {
    OsString::from_vec(bytes)
}

/// A node's full name, split into the directory's shared prefix and the
/// per-entry suffix. Both parts borrow the arena unless a name spans chunks.
#[derive(Clone, Debug)]
pub struct NodeName<'a> {
    prefix: Cow<'a, [u8]>,
    suffix: Cow<'a, [u8]>,
}

impl<'a> NodeName<'a> {
    pub fn to_string_lossy(&self) -> Cow<'_, str> {
        if self.prefix.is_empty() {
            return String::from_utf8_lossy(&self.suffix);
        }
        let mut bytes = Vec::with_capacity(self.prefix.len() + self.suffix.len());
        bytes.extend_from_slice(&self.prefix);
        bytes.extend_from_slice(&self.suffix);
        Cow::Owned(String::from_utf8_lossy(&bytes).into_owned())
    }

    pub fn to_os_string(&self) -> OsString {
        if self.prefix.is_empty() {
            return os_string_from_bytes(self.suffix.to_vec());
        }
        let mut bytes = Vec::with_capacity(self.prefix.len() + self.suffix.len());
        bytes.extend_from_slice(&self.prefix);
        bytes.extend_from_slice(&self.suffix);
        os_string_from_bytes(bytes)
    }

    pub fn bytes(&self) -> impl Iterator<Item = u8> + '_ {
        self.prefix
            .iter()
            .copied()
            .chain(self.suffix.iter().copied())
    }
}

impl fmt::Display for NodeName<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_string_lossy().fmt(f)
    }
}

/// A selected node's path. Built on demand; scans keep no per-node path.
#[derive(Clone, Debug)]
pub struct NodePath {
    path: PathBuf,
}

impl NodePath {
    pub fn display(&self) -> impl fmt::Display + '_ {
        self.path.display()
    }
}

impl fmt::Display for NodePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.path.display().fmt(f)
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
    nodes: Vec<Node>,
    names: NameArena,
    prefixes: NameArena,
    directory_data: Vec<DirectoryData>,
    /// Directory ordinals ordered by `first_child`. Every non-root node id
    /// falls in exactly one directory's contiguous child range.
    by_first_child: Vec<u32>,
    escaped: HashMap<NodeId, u64>,
    hard_link_targets: HashMap<NodeId, NodeId>,
    pub issues: Vec<ScanIssue>,
    pub files: usize,
    pub directories: usize,
    pub symlinks: usize,
    pub hard_links: usize,
}

#[derive(Debug)]
struct DirectoryData {
    bytes: u64,
    first_child: u32,
    child_count: u32,
    pending: u32,
    node: u32,
    prefix_offset: u32,
    errors: u16,
    prefix_len: u8,
    overflow: bool,
}

impl DirectoryData {
    fn new(node: usize, bytes: u64) -> Self {
        Self {
            bytes,
            first_child: 0,
            child_count: 0,
            pending: 1,
            node: node as u32,
            prefix_offset: 0,
            errors: 0,
            prefix_len: NO_PREFIX,
            overflow: false,
        }
    }
}

impl ScanResults {
    #[cfg(test)]
    pub fn apply(&mut self, event: ScanEvent) {
        self.apply_batch([event]);
    }

    pub fn scanning() -> Self {
        Self {
            state: ScanState::Scanning,
            ..Self::default()
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    fn push_node(&mut self, node: Node) {
        if self.nodes.len() == self.nodes.capacity() {
            self.nodes.reserve_exact(NODE_CHUNK);
        }
        self.nodes.push(node);
    }

    pub fn kind(&self, id: NodeId) -> EntryKind {
        self.nodes[id].kind()
    }

    pub fn bytes(&self, id: NodeId) -> u64 {
        let node = self.nodes[id];
        match node.kind() {
            EntryKind::Directory => self.directory_data[node.ordinal()].bytes,
            _ if node.escaped() => self.escaped.get(&id).copied().unwrap_or(0),
            _ => (node.info & PAYLOAD_MASK) as u64 * BLOCK_SIZE,
        }
    }

    pub fn size_overflow(&self, id: NodeId) -> bool {
        match self.nodes[id].kind() {
            EntryKind::Directory => self.directory_data[self.nodes[id].ordinal()].overflow,
            _ => false,
        }
    }

    fn directory_ordinal(&self, id: NodeId) -> Option<usize> {
        let node = self.nodes.get(id)?;
        (node.kind() == EntryKind::Directory).then(|| node.ordinal())
    }

    fn first_child(&self, ordinal: usize) -> usize {
        self.directory_data[ordinal].first_child as usize
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        if id == 0 || self.by_first_child.is_empty() {
            return None;
        }
        let position = self
            .by_first_child
            .partition_point(|&ordinal| self.first_child(ordinal as usize) <= id);
        if position == 0 {
            return None;
        }
        let ordinal = self.by_first_child[position - 1] as usize;
        let directory = &self.directory_data[ordinal];
        debug_assert!(id < directory.first_child as usize + directory.child_count as usize);
        Some(directory.node as usize)
    }

    pub fn hard_link_to(&self, id: NodeId) -> Option<NodeId> {
        self.hard_link_targets.get(&id).copied()
    }

    pub fn child_ids(&self, id: NodeId) -> Range<NodeId> {
        match self.directory_ordinal(id) {
            Some(ordinal) => {
                let directory = &self.directory_data[ordinal];
                let start = directory.first_child as usize;
                start..start + directory.child_count as usize
            }
            None => 0..0,
        }
    }

    pub fn child_count(&self, id: NodeId) -> usize {
        self.directory_ordinal(id).map_or(0, |ordinal| {
            self.directory_data[ordinal].child_count as usize
        })
    }

    #[cfg(test)]
    pub fn pending_directories(&self, id: NodeId) -> usize {
        self.directory_ordinal(id)
            .map_or(0, |ordinal| self.directory_data[ordinal].pending as usize)
    }

    pub fn errors(&self, id: NodeId) -> usize {
        self.directory_ordinal(id)
            .map_or(0, |ordinal| self.directory_data[ordinal].errors as usize)
    }

    pub fn is_complete(&self, id: NodeId) -> bool {
        match self.directory_ordinal(id) {
            Some(ordinal) => {
                let directory = &self.directory_data[ordinal];
                directory.pending == 0 && directory.errors == 0 && !directory.overflow
            }
            None => true,
        }
    }

    fn name_bounds(&self, id: NodeId) -> (usize, usize) {
        let start = self.nodes[id].name as usize;
        let end = match self.nodes.get(id + 1) {
            Some(next) => next.name as usize,
            None => self.names.len(),
        };
        debug_assert!(end >= start, "names must be stored in node order");
        (start, end - start)
    }

    fn prefix(&self, ordinal: usize) -> Cow<'_, [u8]> {
        let directory = &self.directory_data[ordinal];
        if directory.prefix_len == NO_PREFIX || directory.prefix_len == 0 {
            return Cow::Borrowed(&[][..]);
        }
        self.prefixes.read(
            directory.prefix_offset as usize,
            directory.prefix_len as usize,
        )
    }

    pub fn name_suffix(&self, id: NodeId) -> Cow<'_, [u8]> {
        let (start, len) = self.name_bounds(id);
        self.names.read(start, len)
    }

    pub fn name(&self, id: NodeId) -> NodeName<'_> {
        let suffix = self.name_suffix(id);
        let prefix = match self.parent(id) {
            Some(parent) => match self.directory_ordinal(parent) {
                Some(ordinal) => self.prefix(ordinal),
                None => Cow::Borrowed(&[][..]),
            },
            None => Cow::Borrowed(&[][..]),
        };
        NodeName { prefix, suffix }
    }

    pub fn path(&self, id: NodeId) -> NodePath {
        NodePath {
            path: self.to_path_buf(id),
        }
    }

    pub fn to_path_buf(&self, id: NodeId) -> PathBuf {
        let mut names = Vec::new();
        let mut current = Some(id);
        while let Some(node) = current {
            names.push(self.name(node).to_os_string());
            current = self.parent(node);
        }
        let mut names = names.into_iter().rev();
        let mut path = PathBuf::from(names.next().unwrap());
        for name in names {
            path.push(name);
        }
        path
    }

    fn depth(&self, id: NodeId) -> usize {
        let mut depth = 0;
        let mut current = self.parent(id);
        while let Some(node) = current {
            depth += 1;
            current = self.parent(node);
        }
        depth
    }

    pub fn cmp_path(&self, left: NodeId, right: NodeId) -> Ordering {
        if left == right {
            return Ordering::Equal;
        }
        let left_depth = self.depth(left);
        let right_depth = self.depth(right);
        let mut a = left;
        let mut b = right;
        for _ in right_depth..left_depth {
            a = self.parent(a).unwrap();
        }
        for _ in left_depth..right_depth {
            b = self.parent(b).unwrap();
        }
        if a == b {
            return left_depth.cmp(&right_depth);
        }
        while let (Some(a_parent), Some(b_parent)) = (self.parent(a), self.parent(b)) {
            if a_parent == b_parent {
                return self.name(a).bytes().cmp(self.name(b).bytes());
            }
            a = a_parent;
            b = b_parent;
        }
        self.to_path_buf(left).cmp(&self.to_path_buf(right))
    }

    pub fn apply_batch(&mut self, events: impl IntoIterator<Item = ScanEvent>) -> Vec<NodeChange> {
        let first_new = self.nodes.len();
        let already_overflowed = self
            .directory_data
            .first()
            .is_some_and(|root| root.overflow);
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
                    let name = if parent.is_some() {
                        path.file_name().unwrap_or_else(|| path.as_os_str())
                    } else {
                        path.as_os_str()
                    };
                    let name = self.names.push(&os_bytes(name));
                    let pending = usize::from(kind == EntryKind::Directory);
                    self.hard_links += usize::from(hard_link_to.is_some());
                    match kind {
                        EntryKind::File => self.files += 1,
                        EntryKind::Directory => self.directories += 1,
                        EntryKind::Symlink => self.symlinks += 1,
                        EntryKind::Other => {}
                    }
                    let info = match kind {
                        EntryKind::Directory => {
                            assert!(
                                self.directory_data.len() < MAX_DIRECTORIES,
                                "directory count exceeds compact index"
                            );
                            let ordinal = self.directory_data.len();
                            self.directory_data.push(DirectoryData::new(id, bytes));
                            KIND_DIRECTORY | ordinal as u32
                        }
                        EntryKind::File => {
                            let blocks = bytes / BLOCK_SIZE;
                            if bytes % BLOCK_SIZE == 0 && blocks <= MAX_INLINE_BLOCKS {
                                KIND_FILE | blocks as u32
                            } else {
                                self.escaped.insert(id, bytes);
                                KIND_FILE | ESCAPED
                            }
                        }
                        EntryKind::Symlink => KIND_SYMLINK,
                        EntryKind::Other => KIND_OTHER,
                    };
                    self.push_node(Node { name, info });
                    if let Some(parent) = parent {
                        self.attach(parent, id);
                        deltas.entry(parent).or_default().merge(&Delta {
                            bytes,
                            opened: pending as u32,
                            ..Default::default()
                        });
                    }
                    if let Some(target) = hard_link_to {
                        self.hard_link_targets.insert(id, target);
                    }
                }
                ScanEvent::DirectoryRead(id) => {
                    let Some(node) = self.nodes.get(id).copied() else {
                        continue;
                    };
                    if node.kind() != EntryKind::Directory {
                        continue;
                    }
                    self.compress_children(node.ordinal());
                    deltas.entry(id).or_default().closed += 1;
                }
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
            let Some(node) = self.nodes.get(id).copied() else {
                continue;
            };
            if node.kind() != EntryKind::Directory {
                continue;
            }
            let ordinal = node.ordinal();
            let parent = self.parent(id);
            let directory = &mut self.directory_data[ordinal];
            if id < first_new {
                changes.push(NodeChange {
                    id,
                    previous_bytes: Some(directory.bytes),
                });
            }
            directory.overflow |=
                delta.overflow || directory.bytes.checked_add(delta.bytes).is_none();
            directory.bytes = directory.bytes.saturating_add(delta.bytes);
            directory.pending = directory
                .pending
                .saturating_add(delta.opened)
                .saturating_sub(delta.closed);
            directory.errors = directory
                .errors
                .saturating_add(delta.errors.min(u16::MAX as u32) as u16);
            if let Some(parent) = parent {
                deltas.entry(parent).or_default().merge(&delta);
            }
        }
        if self
            .directory_data
            .first()
            .is_some_and(|root| root.overflow && !already_overflowed)
        {
            let path = self.to_path_buf(0);
            self.issues.push(ScanIssue {
                path,
                message:
                    "已分配空间总量超出 u64 统计范围，已停止将溢出目录的大小与占比视为精确值。"
                        .into(),
            });
        }
        if self
            .directory_data
            .first()
            .is_some_and(|root| root.overflow)
            && self.state == ScanState::Complete
        {
            self.state = ScanState::Partial;
        }
        changes.extend((first_new..self.nodes.len()).map(|id| NodeChange {
            id,
            previous_bytes: None,
        }));
        changes
    }

    fn attach(&mut self, parent: NodeId, id: NodeId) {
        let Some(ordinal) = self.directory_ordinal(parent) else {
            panic!("scan entry parent must be a directory");
        };
        let directory = &mut self.directory_data[ordinal];
        if directory.child_count == 0 {
            directory.first_child = id as u32;
            if self.by_first_child.len() == self.by_first_child.capacity() {
                self.by_first_child.reserve_exact(INDEX_CHUNK);
            }
            self.by_first_child.push(ordinal as u32);
        } else {
            debug_assert_eq!(
                directory.first_child as usize + directory.child_count as usize,
                id,
                "scan entries for one directory must be emitted contiguously"
            );
        }
        directory.child_count += 1;
    }

    /// Stores the prefix shared by a directory's children once instead of on
    /// every child name. The children's names are the most recently appended
    /// bytes, so the run can be compacted and truncated in place.
    fn compress_children(&mut self, ordinal: usize) {
        let directory = &self.directory_data[ordinal];
        if directory.prefix_len != NO_PREFIX || directory.child_count < 2 {
            return;
        }
        let first = directory.first_child as usize;
        let count = directory.child_count as usize;
        let (first_start, first_len) = self.name_bounds(first);
        if first_len == 0 {
            self.directory_data[ordinal].prefix_len = 0;
            return;
        }
        let first_name = self.names.read(first_start, first_len).into_owned();
        let mut prefix = first_name.len();
        for id in first + 1..first + count {
            let (start, len) = self.name_bounds(id);
            let name = self.names.read(start, len);
            let limit = prefix.min(name.len());
            let mut index = 0;
            while index < limit && first_name[index] == name[index] {
                index += 1;
            }
            prefix = index;
            if prefix == 0 {
                break;
            }
        }
        let prefix = prefix.min(u8::MAX as usize);
        if prefix == 0 {
            self.directory_data[ordinal].prefix_len = 0;
            return;
        }
        let run_start = self.nodes[first].name as usize;
        let run_end = self.names.len();
        let prefix_offset = self.prefixes.push(&first_name[..prefix]);
        let mut cursor = run_start;
        let mut buffer = vec![0; 1024];
        for id in first..first + count {
            let (source, length) = self.name_bounds(id);
            let suffix = length - prefix;
            if suffix > buffer.len() {
                buffer.resize(suffix, 0);
            }
            self.names.read_into(source + prefix, &mut buffer[..suffix]);
            self.names.write(cursor, &buffer[..suffix]);
            self.nodes[id].name = cursor as u32;
            cursor += suffix;
        }
        debug_assert_eq!(cursor, run_end - prefix * count);
        self.names.truncate(cursor);
        let directory = &mut self.directory_data[ordinal];
        directory.prefix_offset = prefix_offset;
        directory.prefix_len = prefix as u8;
    }
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
    opened: u32,
    closed: u32,
    errors: u32,
}

impl Delta {
    fn merge(&mut self, other: &Self) {
        self.overflow |= other.overflow || self.bytes.checked_add(other.bytes).is_none();
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.opened = self.opened.saturating_add(other.opened);
        self.closed = self.closed.saturating_add(other.closed);
        self.errors = self.errors.saturating_add(other.errors);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DiskSpace {
    pub capacity: u64,
    pub available: u64,
}

/// Convenience for callers that only need the selected root's size.
impl ScanResults {
    pub fn root_bytes(&self) -> u64 {
        if self.is_empty() { 0 } else { self.bytes(0) }
    }

    pub fn root_overflow(&self) -> bool {
        !self.is_empty() && self.size_overflow(0)
    }
}

#[cfg(test)]
mod tests;
