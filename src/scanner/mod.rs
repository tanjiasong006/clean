//! Parallel directory traversal. Workers list one directory at a time; each
//! directory's entries are emitted as one contiguous run so the compact result
//! model can store children as ranges. Allocated file bytes belong to the
//! first emitted (device, inode) path.
use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    models::{EntryKind, ScanEvent, ScanIssue, ScanState},
    platform::scan_fs::{DirEntry, Directory, EntryItem, FileIdentity, FileSystem, RootFileSystem},
};

pub struct ScanJob {
    pub updates: Receiver<Vec<ScanEvent>>,
    cancel: std::sync::Arc<AtomicBool>,
}

impl ScanJob {
    pub fn start(path: PathBuf) -> io::Result<Self> {
        let (sender, updates) = mpsc::sync_channel(8);
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
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

#[cfg(target_os = "macos")]
fn raise_descriptor_limit() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // A worker may hold a parent handle for each directory whose children
        // are still queued. The default soft limit is often only 256.
        unsafe {
            let mut limit = std::mem::zeroed::<libc::rlimit>();
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 {
                let target = limit.rlim_max.min(4096);
                if limit.rlim_cur < target {
                    limit.rlim_cur = target;
                    let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
                }
            }
        }
    });
}

#[cfg(not(target_os = "macos"))]
fn raise_descriptor_limit() {}

fn scan_real(root: &Path, cancel: &AtomicBool, emit: &mut impl FnMut(ScanEvent) -> bool) {
    raise_descriptor_limit();
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

fn worker_count() -> usize {
    #[cfg(test)]
    if let Some(count) = std::env::var("CLEAN_SCAN_WORKERS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        return count;
    }
    thread::available_parallelism()
        .map_or(1, |count| count.get())
        .clamp(1, 16)
}

fn scan(
    root: &Path,
    cancel: &AtomicBool,
    fs: &dyn FileSystem,
    emit: &mut impl FnMut(ScanEvent) -> bool,
) {
    let metadata = fs.root_metadata();
    if !emit(ScanEvent::Entry {
        path: root.into(),
        parent: None,
        kind: EntryKind::Directory,
        bytes: metadata.allocated_bytes,
        hard_link_to: None,
    }) {
        return;
    }

    let root_directory = match fs.open_root() {
        Ok(directory) => directory,
        Err(error) => {
            if emit(ScanEvent::Issue {
                parent: 0,
                issue: ScanIssue {
                    path: root.into(),
                    message: error.to_string(),
                },
            }) {
                emit(ScanEvent::DirectoryRead(0));
                emit(ScanEvent::Finished(ScanState::Failed));
            }
            return;
        }
    };

    let workers = worker_count();
    let (sender, events) = mpsc::sync_channel::<Vec<ScanEvent>>(workers * 2);
    let shared = Shared {
        cancel,
        work: Mutex::new(WorkState {
            queue: VecDeque::from([WorkItem {
                path: root.into(),
                id: 0,
                source: WorkSource::Open(Arc::from(root_directory)),
            }]),
            pending: 1,
            workers_left: workers,
            stopped: false,
            root_failed: false,
            errors: false,
        }),
        ready: Condvar::new(),
        stopped: AtomicBool::new(false),
        emit: Mutex::new(EmitState {
            next_id: 1,
            hard_links: HashMap::new(),
            sender: Some(sender),
        }),
    };

    let mut disconnected = false;
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| worker(&shared));
        }
        'replay: loop {
            if cancel.load(Ordering::Relaxed) || shared.stopped.load(Ordering::Relaxed) {
                break 'replay;
            }
            let unit = match events.recv() {
                Ok(unit) => unit,
                Err(_) => break 'replay,
            };
            for event in unit {
                if cancel.load(Ordering::Relaxed) || shared.stopped.load(Ordering::Relaxed) {
                    break 'replay;
                }
                if !emit(event) {
                    disconnected = true;
                    shared.stop();
                    break 'replay;
                }
            }
        }
        // Closing the receive side lets blocked senders fail instead of
        // stalling the scope's join when traversal is interrupted.
        drop(events);
    });

    if disconnected {
        return;
    }
    let (root_failed, errors) = {
        let work = shared
            .work
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        (work.root_failed, work.errors)
    };
    let state = if root_failed {
        ScanState::Failed
    } else if errors || cancel.load(Ordering::Relaxed) {
        ScanState::Partial
    } else {
        ScanState::Complete
    };
    emit(ScanEvent::Finished(state));
}

struct WorkItem {
    path: PathBuf,
    id: u32,
    source: WorkSource,
}

enum WorkSource {
    /// An already open directory (the scan root).
    Open(Arc<dyn Directory>),
    /// A child to open from its parent's handle. Holding the parent handle
    /// instead of a full path keeps each open to one path component.
    Child {
        parent: Arc<dyn Directory>,
        name: OsString,
        identity: FileIdentity,
    },
}

/// A worker's private stack also receives children when the shared queue is
/// full, so one wide directory can never make a producer block while all
/// workers are producing. Both stacks are LIFO: keeping traversal depth-first
/// also keeps the number of retained parent handles near the current depth.
const MAX_QUEUED: usize = 512;

struct WorkState {
    queue: VecDeque<WorkItem>,
    /// Directories discovered but not yet fully processed, including the ones
    /// currently being read. Reaching zero means traversal is over.
    pending: usize,
    workers_left: usize,
    stopped: bool,
    root_failed: bool,
    errors: bool,
}

struct EmitState {
    next_id: u32,
    hard_links: HashMap<FileIdentity, u32>,
    /// Dropped when traversal ends so the replay loop's receive terminates.
    sender: Option<SyncSender<Vec<ScanEvent>>>,
}

struct Shared<'a> {
    cancel: &'a AtomicBool,
    work: Mutex<WorkState>,
    ready: Condvar,
    stopped: AtomicBool,
    emit: Mutex<EmitState>,
}

impl Shared<'_> {
    fn should_stop(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        let mut work = self.work.lock().unwrap_or_else(|error| error.into_inner());
        work.stopped = true;
        self.ready.notify_all();
    }

    fn next_work(&self) -> Option<WorkItem> {
        let mut work = self.work.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if work.stopped
                || self.should_stop()
                || self.cancel.load(Ordering::Relaxed)
                || work.pending == 0
            {
                return None;
            }
            if let Some(item) = work.queue.pop_back() {
                return Some(item);
            }
            work = self
                .ready
                .wait(work)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    fn complete_one(&self) {
        let done = {
            let mut work = self.work.lock().unwrap_or_else(|error| error.into_inner());
            work.pending = work.pending.saturating_sub(1);
            let done = work.pending == 0;
            if done {
                self.ready.notify_all();
            }
            done
        };
        if done {
            self.close_sender();
        }
    }

    fn worker_exited(&self) {
        let last = {
            let mut work = self.work.lock().unwrap_or_else(|error| error.into_inner());
            work.workers_left = work.workers_left.saturating_sub(1);
            let last = work.workers_left == 0;
            if last {
                self.ready.notify_all();
            }
            last
        };
        if last {
            self.close_sender();
        }
    }

    fn close_sender(&self) {
        self.emit
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .sender
            .take();
    }

    fn enqueue(&self, children: Vec<WorkItem>, local: &mut Vec<WorkItem>) {
        if children.is_empty() {
            return;
        }
        let mut work = self.work.lock().unwrap_or_else(|error| error.into_inner());
        if work.stopped || self.should_stop() {
            return;
        }
        work.pending += children.len();
        if work.queue.len() >= MAX_QUEUED {
            local.extend(children);
        } else {
            work.queue.extend(children);
        }
        self.ready.notify_all();
    }

    fn mark_errors(&self, root: bool) {
        let mut work = self.work.lock().unwrap_or_else(|error| error.into_inner());
        work.errors = true;
        if root {
            work.root_failed = true;
        }
    }

    fn process(&self, item: WorkItem, local: &mut Vec<WorkItem>) {
        if self.should_stop() || self.cancel.load(Ordering::Relaxed) {
            return;
        }
        let directory: Arc<dyn Directory> = match &item.source {
            WorkSource::Open(directory) => directory.clone(),
            WorkSource::Child {
                parent,
                name,
                identity,
            } => match parent.open_child(name, *identity) {
                Ok(directory) => Arc::from(directory),
                Err(error) => {
                    self.report_issue(&item, error);
                    return;
                }
            },
        };
        let entries = match directory.entries() {
            Ok(entries) => entries,
            Err(error) => {
                self.report_issue(&item, error);
                return;
            }
        };

        let mut collected = Vec::new();
        let mut issues = Vec::new();
        let mut interrupted = false;
        let mut entries = entries;
        loop {
            if self.should_stop() || self.cancel.load(Ordering::Relaxed) {
                interrupted = true;
                break;
            }
            let Some(entry) = entries.next() else { break };
            if self.should_stop() || self.cancel.load(Ordering::Relaxed) {
                interrupted = true;
                break;
            }
            match entry {
                EntryItem::Entry(entry) => collected.push(entry),
                EntryItem::Error(error) => {
                    let path = match error.name {
                        Some(name) => item.path.join(name),
                        None => item.path.clone(),
                    };
                    issues.push((path, error.error));
                }
            }
        }
        if self.should_stop() {
            return;
        }
        let had_issues = !issues.is_empty();
        let children = self.emit_entries(&item, &directory, collected, issues, !interrupted);
        if had_issues {
            // A failed entry only makes the result partial; a failure to open
            // or list the root is what makes the whole scan failed.
            self.mark_errors(false);
        }
        if interrupted {
            return;
        }
        if let Some(children) = children {
            self.enqueue(children, local);
        }
    }

    fn report_issue(&self, item: &WorkItem, error: io::Error) {
        #[cfg(test)]
        eprintln!(
            "ISSUE id={} path={} error={}",
            item.id,
            item.path.display(),
            error
        );
        if self.should_stop() {
            return;
        }
        let id = item.id as usize;
        let events = vec![
            ScanEvent::Issue {
                parent: id,
                issue: ScanIssue {
                    path: item.path.clone(),
                    message: error.to_string(),
                },
            },
            ScanEvent::DirectoryRead(id),
        ];
        if !self.send(events) {
            return;
        }
        self.mark_errors(item.id == 0);
    }

    /// Assigns node ids and sends one directory's events as a contiguous run.
    /// Returns child directories to schedule once their ids are known.
    fn emit_entries(
        &self,
        item: &WorkItem,
        directory: &Arc<dyn Directory>,
        entries: Vec<DirEntry>,
        issues: Vec<(PathBuf, io::Error)>,
        close: bool,
    ) -> Option<Vec<WorkItem>> {
        let mut emit = self.emit.lock().unwrap_or_else(|error| error.into_inner());
        let sender = emit.sender.as_ref()?.clone();
        let base = emit.next_id;
        let count = u32::try_from(entries.len()).ok()?;
        emit.next_id = base.checked_add(count)?;
        let mut events = Vec::with_capacity(entries.len() + issues.len() + 1);
        let mut children = Vec::new();
        for (index, entry) in entries.into_iter().enumerate() {
            let id = base + index as u32;
            let kind = entry.metadata.kind;
            let hard_link_to = if kind == EntryKind::File && entry.metadata.links > 1 {
                match emit.hard_links.entry(entry.metadata.identity) {
                    Entry::Occupied(entry) => Some(*entry.get() as usize),
                    Entry::Vacant(entry) => {
                        entry.insert(id);
                        None
                    }
                }
            } else {
                None
            };
            let bytes = if hard_link_to.is_some() {
                0
            } else {
                entry.metadata.allocated_bytes
            };
            if kind == EntryKind::Directory {
                children.push(WorkItem {
                    path: item.path.join(&entry.name),
                    id,
                    source: WorkSource::Child {
                        parent: directory.clone(),
                        name: entry.name.clone(),
                        identity: entry.metadata.identity,
                    },
                });
            }
            events.push(ScanEvent::Entry {
                path: PathBuf::from(entry.name),
                parent: Some(item.id as usize),
                kind,
                bytes,
                hard_link_to,
            });
        }
        for (path, error) in issues {
            events.push(ScanEvent::Issue {
                parent: item.id as usize,
                issue: ScanIssue {
                    path,
                    message: error.to_string(),
                },
            });
        }
        if close {
            events.push(ScanEvent::DirectoryRead(item.id as usize));
        }
        if sender.send(events).is_err() {
            emit.sender = None;
            drop(emit);
            self.stop();
            return None;
        }
        Some(children)
    }

    fn send(&self, events: Vec<ScanEvent>) -> bool {
        let mut emit = self.emit.lock().unwrap_or_else(|error| error.into_inner());
        let Some(sender) = emit.sender.as_ref() else {
            return false;
        };
        if sender.send(events).is_err() {
            emit.sender = None;
            drop(emit);
            self.stop();
            return false;
        }
        true
    }
}

fn worker(shared: &Shared<'_>) {
    let mut local = Vec::new();
    loop {
        let item = match local.pop() {
            Some(item) => item,
            None => match shared.next_work() {
                Some(item) => item,
                None => break,
            },
        };
        shared.process(item, &mut local);
        shared.complete_one();
    }
    shared.worker_exited();
}

#[cfg(test)]
mod tests;
