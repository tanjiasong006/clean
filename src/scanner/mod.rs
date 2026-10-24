//! One traversal worker, a bounded channel and batches of at most 128 events.
//! Allocated file bytes belong to the first observed (device, inode) path.
use std::{
    collections::{HashMap, hash_map::Entry},
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use crate::{
    models::{EntryKind, ScanEvent, ScanIssue, ScanState},
    platform::scan_fs::{FileIdentity, FileSystem, RootFileSystem},
};

#[cfg(unix)]
fn name_bytes(name: &OsStr) -> &[u8] {
    name.as_bytes()
}

#[cfg(not(unix))]
fn name_bytes(name: &OsStr) -> Vec<u8> {
    name.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn os_from_bytes(bytes: &[u8]) -> OsString {
    OsString::from_vec(bytes.to_vec())
}

#[cfg(not(unix))]
fn os_from_bytes(bytes: &[u8]) -> OsString {
    String::from_utf8_lossy(bytes).into_owned().into()
}

const NO_PARENT: u32 = u32::MAX;

/// Pending directories store a parent handle and a name instead of a full
/// path. Wide trees therefore cost one short name per discovered directory,
/// not one copied absolute path.
#[derive(Default)]
struct PendingDirectories {
    entries: Vec<PendingDirectory>,
    names: Vec<u8>,
    stack: Vec<u32>,
}

struct PendingDirectory {
    parent: u32,
    id: u32,
    name_offset: u32,
    name_len: u16,
    identity: FileIdentity,
}

impl PendingDirectories {
    fn push(&mut self, parent: u32, id: u32, name: &OsStr, identity: FileIdentity) {
        let bytes = name_bytes(name);
        assert!(
            self.entries.len() < u32::MAX as usize,
            "scan discovered too many directories"
        );
        assert!(
            bytes.len() <= u16::MAX as usize,
            "directory name exceeds compact path storage"
        );
        assert!(
            self.names.len() + bytes.len() <= u32::MAX as usize,
            "pending directory names exceed compact path storage"
        );
        let name_offset = self.names.len() as u32;
        self.names.extend_from_slice(bytes);
        let handle = self.entries.len() as u32;
        self.entries.push(PendingDirectory {
            parent,
            id,
            name_offset,
            name_len: bytes.len() as u16,
            identity,
        });
        self.stack.push(handle);
    }

    fn pop(&mut self) -> Option<(u32, u32, PathBuf, FileIdentity)> {
        let handle = self.stack.pop()?;
        let entry = &self.entries[handle as usize];
        Some((handle, entry.id, self.path(handle), entry.identity))
    }

    fn path(&self, handle: u32) -> PathBuf {
        let mut parts = Vec::new();
        let mut current = handle;
        loop {
            let entry = &self.entries[current as usize];
            let start = entry.name_offset as usize;
            parts.push(&self.names[start..start + entry.name_len as usize]);
            if entry.parent == NO_PARENT {
                break;
            }
            current = entry.parent;
        }
        let mut parts = parts.into_iter().rev();
        let mut path = PathBuf::from(os_from_bytes(parts.next().expect("root name")));
        for part in parts {
            path.push(os_from_bytes(part));
        }
        path
    }
}

pub struct ScanJob {
    pub updates: Receiver<Vec<ScanEvent>>,
    cancel: Arc<AtomicBool>,
}

impl ScanJob {
    pub fn start(path: PathBuf) -> io::Result<Self> {
        let (sender, updates) = mpsc::sync_channel(8);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        thread::Builder::new()
            .name("directory-scanner".into())
            .spawn(move || {
                let mut output = Batches::new(sender);
                scan_real(&path, &worker_cancel, &mut |event| output.push(event));
                output.flush();
            })?;
        Ok(Self { updates, cancel })
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for ScanJob {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct Batches {
    sender: SyncSender<Vec<ScanEvent>>,
    pending: Vec<ScanEvent>,
    last_flush: Instant,
}

impl Batches {
    fn new(sender: SyncSender<Vec<ScanEvent>>) -> Self {
        Self {
            sender,
            pending: Vec::with_capacity(128),
            last_flush: Instant::now(),
        }
    }

    fn push(&mut self, event: ScanEvent) -> bool {
        let finished = matches!(event, ScanEvent::Finished(_));
        self.pending.push(event);
        if finished
            || self.pending.len() >= 128
            || self.last_flush.elapsed() >= Duration::from_millis(80)
        {
            self.flush()
        } else {
            true
        }
    }

    fn flush(&mut self) -> bool {
        if self.pending.is_empty() {
            return true;
        }
        self.last_flush = Instant::now();
        let batch = std::mem::replace(&mut self.pending, Vec::with_capacity(128));
        self.sender.send(batch).is_ok()
    }
}

fn scan_real(root: &Path, cancel: &AtomicBool, emit: &mut impl FnMut(ScanEvent) -> bool) {
    if cancel.load(Ordering::Relaxed) {
        emit(ScanEvent::Finished(ScanState::Partial));
        return;
    }
    match RootFileSystem::open(root) {
        Ok(fs) => scan(root, cancel, &fs, emit),
        Err(error) => {
            if emit(ScanEvent::Issue {
                parent: 0,
                issue: ScanIssue {
                    path: root.into(),
                    message: error.to_string(),
                },
            }) {
                emit(ScanEvent::Finished(ScanState::Failed));
            }
        }
    }
}

fn scan(
    root: &Path,
    cancel: &AtomicBool,
    fs: &dyn FileSystem,
    emit: &mut impl FnMut(ScanEvent) -> bool,
) {
    if !emit(ScanEvent::Entry {
        path: root.into(),
        parent: None,
        kind: EntryKind::Directory,
        bytes: fs.root_metadata().allocated_bytes,
        hard_link_to: None,
    }) {
        return;
    }
    let mut allocated_files = HashMap::new();
    let mut next_id = 1;
    let mut directories = PendingDirectories::default();
    directories.push(NO_PARENT, 0, root.as_os_str(), fs.root_metadata().identity);
    let mut errors = false;
    let mut root_failed = false;
    while let Some((handle, parent, directory, identity)) = directories.pop() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let mut entries = match fs.read_dir(&directory, identity) {
            Ok(entries) => entries,
            Err(error) => {
                errors = true;
                root_failed |= parent == 0;
                if !emit(ScanEvent::Issue {
                    parent: parent as usize,
                    issue: ScanIssue {
                        path: directory,
                        message: error.to_string(),
                    },
                }) {
                    return;
                }
                if !emit(ScanEvent::DirectoryRead(parent as usize)) {
                    return;
                }
                continue;
            }
        };
        let mut interrupted = false;
        loop {
            // Check before advancing the iterator, not after a potentially
            // blocking filesystem read or an additional metadata request.
            if cancel.load(Ordering::Relaxed) {
                interrupted = true;
                break;
            }
            let Some(entry) = entries.next() else { break };
            if cancel.load(Ordering::Relaxed) {
                interrupted = true;
                break;
            }
            let observed = entry
                .map_err(|error| (directory.clone(), OsString::new(), error))
                .and_then(|name| {
                    let path = directory.join(&name);
                    match entries.metadata(&name) {
                        Ok(metadata) => Ok((path, name, metadata)),
                        Err(error) => Err((path, name, error)),
                    }
                });
            match observed {
                Ok((path, name, metadata)) => {
                    let kind = metadata.kind;
                    // Only multiply linked inodes can repeat inside one scan.
                    let hard_link_to = if kind == EntryKind::File && metadata.links > 1 {
                        match allocated_files.entry(metadata.identity) {
                            Entry::Occupied(entry) => Some(*entry.get()),
                            Entry::Vacant(entry) => {
                                entry.insert(next_id);
                                None
                            }
                        }
                    } else {
                        None
                    };
                    let bytes = if hard_link_to.is_some() {
                        0
                    } else {
                        metadata.allocated_bytes
                    };
                    if kind == EntryKind::Directory {
                        directories.push(handle, next_id as u32, &name, metadata.identity);
                    }
                    next_id += 1;
                    if !emit(ScanEvent::Entry {
                        path,
                        parent: Some(parent as usize),
                        kind,
                        bytes,
                        hard_link_to,
                    }) {
                        return;
                    }
                }
                Err((path, _, error)) => {
                    errors = true;
                    if !emit(ScanEvent::Issue {
                        parent: parent as usize,
                        issue: ScanIssue {
                            path,
                            message: error.to_string(),
                        },
                    }) {
                        return;
                    }
                }
            }
        }
        if !interrupted && !emit(ScanEvent::DirectoryRead(parent as usize)) {
            return;
        }
    }
    emit(ScanEvent::Finished(if root_failed {
        ScanState::Failed
    } else if errors || cancel.load(Ordering::Relaxed) {
        ScanState::Partial
    } else {
        ScanState::Complete
    }));
}

#[cfg(test)]
mod tests;
