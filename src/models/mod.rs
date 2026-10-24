//! Compact scan results. A node is eight bytes: a dictionary id for its name
//! and a packed kind/payload word. Names are interned by content, so a name
//! repeated anywhere in the scan is stored once; directory metadata lives in a
//! separate table. Entries of one directory must be emitted contiguously so
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

const PAYLOAD_BITS: u32 = 22;
const KIND_FILE: u32 = 0 << PAYLOAD_BITS;
const KIND_DIRECTORY: u32 = 1 << PAYLOAD_BITS;
const KIND_SYMLINK: u32 = 2 << PAYLOAD_BITS;
const KIND_OTHER: u32 = 3 << PAYLOAD_BITS;
const KIND_MASK: u32 = 3 << PAYLOAD_BITS;
const PAYLOAD_MASK: u32 = (1 << PAYLOAD_BITS) - 1;
/// A file whose block count does not fit the inline field stores its exact byte
/// total in a side map.
const ESCAPED_BLOCKS: u32 = PAYLOAD_MASK;

const BLOCK_SIZE: u64 = 512;
/// Files at or above roughly 2 GiB keep their exact byte total in the side map.
const MAX_INLINE_BLOCKS: u64 = (PAYLOAD_MASK - 1) as u64;
const MAX_DIRECTORIES: usize = 1 << PAYLOAD_BITS;
/// Nodes hold 24-bit dictionary ids.
const MAX_NAMES: usize = 1 << 24;

/// Directory records hold a read-error count and an overflow flag in a sparse
/// map; both are rare, so the base record stays 24 bytes.
const DIRECTORY_ERRORS_MASK: u32 = u16::MAX as u32;
const DIRECTORY_OVERFLOW: u32 = 1 << 16;

const NAME_CHUNK: usize = 1 << 20;
const NODE_CHUNK: usize = 1 << 16;
const INDEX_CHUNK: usize = 1 << 16;
/// Names are grouped so a dictionary entry's arena offset can be found without
/// storing four bytes of offset for every entry.
const NAME_BLOCK: usize = 32;
/// A dictionary length byte; entries that long keep their length in a side map.
const LONG_NAME: u8 = u8::MAX;

/// A node occupies six bytes: a 24-bit dictionary id for the entry's name and
/// a 24-bit kind/payload word. Fields are byte arrays so the in-memory layout
/// stays six bytes without an alignment gap.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Node {
    name: [u8; 3],
    info: [u8; 3],
}

impl Node {
    fn new(name: u32, info: u32) -> Self {
        debug_assert!((name as usize) < MAX_NAMES);
        debug_assert!(info < 1 << 24);
        let name = name.to_le_bytes();
        let info = info.to_le_bytes();
        Self {
            name: [name[0], name[1], name[2]],
            info: [info[0], info[1], info[2]],
        }
    }

    /// Dictionary id of the entry's name.
    fn name_id(self) -> u32 {
        u24(self.name)
    }

    fn info_bits(self) -> u32 {
        u24(self.info)
    }

    fn kind(self) -> EntryKind {
        match self.info_bits() & KIND_MASK {
            KIND_DIRECTORY => EntryKind::Directory,
            KIND_SYMLINK => EntryKind::Symlink,
            KIND_OTHER => EntryKind::Other,
            _ => EntryKind::File,
        }
    }

    fn ordinal(self) -> usize {
        (self.info_bits() & PAYLOAD_MASK) as usize
    }

    fn escaped(self) -> bool {
        self.kind() == EntryKind::File && self.info_bits() & PAYLOAD_MASK == ESCAPED_BLOCKS
    }
}

fn u24(bytes: [u8; 3]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], 0])
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("name", &self.name_id())
            .field("info", &self.info_bits())
            .finish()
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

    fn shrink_to_fit(&mut self) {
        if let Some(last) = self.chunks.last_mut() {
            last.shrink_to_fit();
        }
    }
}

fn hash_name(name: &[u8]) -> u64 {
    // FNV-1a: cheap over short names and adequate for open addressing.
    let mut hash = 0xcbf29ce484222325_u64;
    for &byte in name {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Interned entry names. Each distinct name is stored once; a node holds its
/// dictionary id. Lengths are one byte per entry and every 32nd name records an
/// arena offset, so the index costs about 1.1 bytes per distinct name instead
/// of a four-byte offset each. The lookup table only exists while scanning.
#[derive(Default)]
struct NameDict {
    bytes: NameArena,
    lengths: Vec<u8>,
    block_offsets: Vec<u32>,
    long_lengths: HashMap<u32, u32>,
    table: Vec<u32>,
}

impl NameDict {
    #[cfg(test)]
    fn len(&self) -> usize {
        self.lengths.len()
    }

    #[cfg(test)]
    fn byte_len(&self) -> usize {
        self.bytes.len()
    }

    fn intern(&mut self, name: &[u8]) -> u32 {
        if self.table.is_empty() {
            self.table = vec![0; 1024];
        }
        if self.lengths.len() * 4 >= self.table.len() * 3 {
            self.grow();
        }
        let mask = self.table.len() - 1;
        let mut index = hash_name(name) as usize & mask;
        loop {
            let slot = self.table[index];
            if slot == 0 {
                break;
            }
            let id = slot - 1;
            if &*self.read(id) == name {
                return id;
            }
            index = (index + 1) & mask;
        }
        let id = self.push(name);
        self.table[index] = id + 1;
        id
    }

    fn read(&self, id: u32) -> Cow<'_, [u8]> {
        let (offset, length) = self.range(id);
        self.bytes.read(offset, length)
    }

    fn range(&self, id: u32) -> (usize, usize) {
        let id = id as usize;
        let block = id / NAME_BLOCK;
        let mut offset = self.block_offsets[block] as usize;
        for previous in block * NAME_BLOCK..id {
            offset += self.length(previous as u32) as usize;
        }
        (offset, self.length(id as u32) as usize)
    }

    fn length(&self, id: u32) -> u32 {
        match self.lengths[id as usize] {
            LONG_NAME => self.long_lengths[&id],
            length => length as u32,
        }
    }

    fn push(&mut self, name: &[u8]) -> u32 {
        let id = self.lengths.len();
        assert!(
            id < MAX_NAMES,
            "scan contains too many unique names for 24-bit node ids"
        );
        if id.is_multiple_of(NAME_BLOCK) {
            self.block_offsets.push(self.bytes.len() as u32);
        }
        self.bytes.push(name);
        if name.len() < LONG_NAME as usize {
            self.lengths.push(name.len() as u8);
        } else {
            self.lengths.push(LONG_NAME);
            self.long_lengths.insert(id as u32, name.len() as u32);
        }
        id as u32
    }

    fn grow(&mut self) {
        let capacity = (self.table.len() * 2).max(1024);
        let mut table = vec![0; capacity];
        let mask = capacity - 1;
        for id in 0..self.lengths.len() as u32 {
            let (offset, length) = self.range(id);
            let mut index = hash_name(&self.bytes.read(offset, length)) as usize & mask;
            while table[index] != 0 {
                index = (index + 1) & mask;
            }
            table[index] = id + 1;
        }
        self.table = table;
    }

    fn finish(&mut self) {
        // Interning is over; only the compact dictionary stays resident.
        self.table = Vec::new();
        self.long_lengths.shrink_to_fit();
        self.lengths.shrink_to_fit();
        self.block_offsets.shrink_to_fit();
        self.bytes.shrink_to_fit();
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

/// A node's name, borrowed from the shared name dictionary unless it spans
/// arena chunks.
#[derive(Clone, Debug)]
pub struct NodeName<'a> {
    bytes: Cow<'a, [u8]>,
}

impl NodeName<'_> {
    pub fn to_string_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }

    pub fn to_os_string(&self) -> OsString {
        os_string_from_bytes(self.bytes.to_vec())
    }

    pub fn bytes(&self) -> impl Iterator<Item = u8> + '_ {
        self.bytes.iter().copied()
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
    names: NameDict,
    directory_data: Vec<DirectoryData>,
    /// Directory ordinals ordered by `first_child`. Every non-root node id
    /// falls in exactly one directory's contiguous child range.
    by_first_child: Vec<u32>,
    escaped: HashMap<NodeId, u64>,
    /// Per-directory read-error count and overflow flag, keyed by ordinal.
    directory_faults: HashMap<u32, u32>,
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
}

impl DirectoryData {
    fn new(node: usize, bytes: u64) -> Self {
        Self {
            bytes,
            first_child: 0,
            child_count: 0,
            pending: 1,
            node: node as u32,
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

    fn push_node(&mut self, name: u32, info: u32) {
        if self.nodes.len() == self.nodes.capacity() {
            self.nodes.reserve_exact(NODE_CHUNK);
        }
        self.nodes.push(Node::new(name, info));
    }

    pub fn kind(&self, id: NodeId) -> EntryKind {
        self.nodes[id].kind()
    }

    pub fn bytes(&self, id: NodeId) -> u64 {
        let node = self.nodes[id];
        match node.kind() {
            EntryKind::Directory => self.directory_data[node.ordinal()].bytes,
            _ if node.escaped() => self.escaped.get(&id).copied().unwrap_or(0),
            _ => (node.info_bits() & PAYLOAD_MASK) as u64 * BLOCK_SIZE,
        }
    }

    pub fn size_overflow(&self, id: NodeId) -> bool {
        match self.nodes[id].kind() {
            EntryKind::Directory => self.faults(self.nodes[id].ordinal()) & DIRECTORY_OVERFLOW != 0,
            _ => false,
        }
    }

    fn directory_ordinal(&self, id: NodeId) -> Option<usize> {
        let node = self.nodes.get(id)?;
        (node.kind() == EntryKind::Directory).then(|| node.ordinal())
    }

    fn faults(&self, ordinal: usize) -> u32 {
        self.directory_faults
            .get(&(ordinal as u32))
            .copied()
            .unwrap_or(0)
    }

    fn note_faults(&mut self, ordinal: usize, overflow: bool, errors: u32) {
        if !overflow && errors == 0 {
            return;
        }
        let faults = self.directory_faults.entry(ordinal as u32).or_default();
        if overflow {
            *faults |= DIRECTORY_OVERFLOW;
        }
        let errors = (*faults & DIRECTORY_ERRORS_MASK)
            .saturating_add(errors.min(DIRECTORY_ERRORS_MASK))
            .min(DIRECTORY_ERRORS_MASK);
        *faults = (*faults & !DIRECTORY_ERRORS_MASK) | errors;
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
        self.directory_ordinal(id).map_or(0, |ordinal| {
            (self.faults(ordinal) & DIRECTORY_ERRORS_MASK) as usize
        })
    }

    pub fn is_complete(&self, id: NodeId) -> bool {
        match self.directory_ordinal(id) {
            Some(ordinal) => self.directory_data[ordinal].pending == 0 && self.faults(ordinal) == 0,
            None => true,
        }
    }

    pub fn name_bytes(&self, id: NodeId) -> Cow<'_, [u8]> {
        self.names.read(self.nodes[id].name_id())
    }

    pub fn name(&self, id: NodeId) -> NodeName<'_> {
        NodeName {
            bytes: self.name_bytes(id),
        }
    }

    #[cfg(test)]
    pub fn unique_names(&self) -> usize {
        self.names.len()
    }

    #[cfg(test)]
    pub fn stored_name_bytes(&self) -> usize {
        self.names.byte_len()
    }

    #[cfg(test)]
    pub fn name_id(&self, id: NodeId) -> u32 {
        self.nodes[id].name_id()
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
        let already_overflowed = self.faults(0) & DIRECTORY_OVERFLOW != 0;
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
                    let name = self.names.intern(&os_bytes(name));
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
                                KIND_FILE | ESCAPED_BLOCKS
                            }
                        }
                        EntryKind::Symlink => KIND_SYMLINK,
                        EntryKind::Other => KIND_OTHER,
                    };
                    self.push_node(name, info);
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
                    deltas.entry(id).or_default().closed += 1;
                }
                ScanEvent::Issue { parent, issue } => {
                    self.issues.push(issue);
                    if parent < self.nodes.len() {
                        deltas.entry(parent).or_default().errors += 1;
                    }
                }
                ScanEvent::Finished(state) => {
                    self.state = state;
                    // Traversal is over, so capacity reserved while growing is
                    // dead weight: the name dictionary drops its lookup table,
                    // the directory table halves to its actual occupancy, and
                    // the alias tables drop unused buckets.
                    self.names.finish();
                    self.directory_data.shrink_to_fit();
                    self.by_first_child.shrink_to_fit();
                    self.escaped.shrink_to_fit();
                    self.directory_faults.shrink_to_fit();
                    self.hard_link_targets.shrink_to_fit();
                    self.issues.shrink_to_fit();
                }
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
            let overflow = delta.overflow || directory.bytes.checked_add(delta.bytes).is_none();
            directory.bytes = directory.bytes.saturating_add(delta.bytes);
            directory.pending = directory
                .pending
                .saturating_add(delta.opened)
                .saturating_sub(delta.closed);
            self.note_faults(ordinal, overflow, delta.errors);
            if let Some(parent) = parent {
                deltas.entry(parent).or_default().merge(&delta);
            }
        }
        let root_overflow = self.faults(0) & DIRECTORY_OVERFLOW != 0;
        if root_overflow && !already_overflowed {
            let path = self.to_path_buf(0);
            self.issues.push(ScanIssue {
                path,
                message:
                    "已分配空间总量超出 u64 统计范围，已停止将溢出目录的大小与占比视为精确值。"
                        .into(),
            });
        }
        if root_overflow && self.state == ScanState::Complete {
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
