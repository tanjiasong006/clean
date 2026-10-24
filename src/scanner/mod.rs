//! One traversal worker, a bounded channel and batches of at most 128 events.
//! Sizes are logical bytes per file path: hard links count once per directory
//! entry. Symlinks and special files contribute zero bytes and are not followed.
use std::{
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

use crate::{
    models::{EntryKind, ScanEvent, ScanIssue, ScanState},
    platform::scan_fs::{FileSystem, RootFileSystem},
};

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
        bytes: 0,
    }) {
        return;
    }
    let mut next_id = 1;
    let mut directories = vec![(0, root.to_path_buf(), fs.root_metadata().identity)];
    let mut errors = false;
    let mut root_failed = false;
    while let Some((parent, directory, identity)) = directories.pop() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let mut entries = match fs.read_dir(&directory, identity) {
            Ok(entries) => entries,
            Err(error) => {
                errors = true;
                root_failed |= parent == 0;
                if !emit(ScanEvent::Issue {
                    parent,
                    issue: ScanIssue {
                        path: directory,
                        message: error.to_string(),
                    },
                }) {
                    return;
                }
                if !emit(ScanEvent::DirectoryRead(parent)) {
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
                .map_err(|error| (directory.clone(), error))
                .and_then(|name| {
                    let path = directory.join(&name);
                    match entries.metadata(&name) {
                        Ok(metadata) => Ok((path, metadata)),
                        Err(error) => Err((path, error)),
                    }
                });
            match observed {
                Ok((path, metadata)) => {
                    let kind = metadata.kind;
                    let bytes = metadata.bytes;
                    if kind == EntryKind::Directory {
                        directories.push((next_id, path.clone(), metadata.identity));
                    }
                    next_id += 1;
                    if !emit(ScanEvent::Entry {
                        path,
                        parent: Some(parent),
                        kind,
                        bytes,
                    }) {
                        return;
                    }
                }
                Err((path, error)) => {
                    errors = true;
                    if !emit(ScanEvent::Issue {
                        parent,
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
        if !interrupted && !emit(ScanEvent::DirectoryRead(parent)) {
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
