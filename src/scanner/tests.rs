use std::{
    ffi::OsStr,
    fs,
    sync::{Arc, atomic::AtomicUsize},
};

use super::*;
use crate::{
    analysis::sorted_children,
    models::{NodeId, ScanResults},
    platform::scan_fs::{Directory, EntryError, EntryItem, EntryMetadata, FileIdentity},
};

struct TempDirectory(PathBuf);

impl TempDirectory {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "clean-scan-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn file(&self, relative: &str, size: usize) -> PathBuf {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![0; size]).unwrap();
        path
    }
}

impl Drop for TempDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn collect_real(root: &Path) -> ScanResults {
    let mut results = ScanResults::scanning();
    scan_real(root, &AtomicBool::new(false), &mut |event| {
        results.apply(event);
        true
    });
    results
}

fn collect(root: &Path, fs: &dyn FileSystem) -> ScanResults {
    let mut results = ScanResults::scanning();
    scan(root, &AtomicBool::new(false), fs, &mut |event| {
        results.apply(event);
        true
    });
    results
}

fn node_id(results: &ScanResults, path: &Path) -> NodeId {
    (0..results.len())
        .find(|id| results.to_path_buf(*id) == path)
        .unwrap()
}

fn node<'a>(results: &'a ScanResults, path: &Path) -> NodeView<'a> {
    NodeView {
        results,
        id: node_id(results, path),
    }
}

struct NodeView<'a> {
    results: &'a ScanResults,
    id: NodeId,
}

impl NodeView<'_> {
    fn bytes(&self) -> u64 {
        self.results.bytes(self.id)
    }

    fn errors(&self) -> usize {
        self.results.errors(self.id)
    }

    fn children(&self) -> std::ops::Range<NodeId> {
        self.results.child_ids(self.id)
    }

    fn is_complete(&self) -> bool {
        self.results.is_complete(self.id)
    }
}

#[cfg(unix)]
fn allocated(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path)
        .unwrap()
        .blocks()
        .checked_mul(512)
        .unwrap()
}

#[cfg(windows)]
fn allocated(path: &Path) -> u64 {
    crate::platform::scan_fs::windows_allocated_bytes(path).unwrap()
}

#[cfg(unix)]
fn link_count(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path).unwrap().nlink()
}

#[cfg(windows)]
fn link_count(path: &Path) -> u64 {
    crate::platform::scan_fs::windows_link_count(path).unwrap()
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

#[cfg(unix)]
fn mark_sparse(_: &fs::File) -> bool {
    true
}

#[cfg(windows)]
fn mark_sparse(file: &fs::File) -> bool {
    // Without sparse support, extending to the test length would allocate it.
    crate::platform::scan_fs::windows_mark_sparse(file).is_ok()
}

#[cfg(unix)]
fn extended_ids_supported() -> bool {
    true
}

#[cfg(windows)]
fn extended_ids_supported() -> bool {
    let probe = TempDirectory::new();
    crate::platform::scan_fs::windows_extended_directory_ids(&probe.0)
}

#[cfg(unix)]
fn symlinks_supported() -> bool {
    true
}

#[cfg(windows)]
fn symlinks_supported() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let probe = TempDirectory::new();
        let target = probe.0.join("target");
        if fs::create_dir(&target).is_err() {
            return false;
        }
        symlink_dir(&target, &probe.0.join("link")).is_ok()
    })
}

fn assert_allocated_totals(results: &ScanResults) {
    for id in 0..results.len() {
        let kind = results.kind(id);
        let own = if matches!(kind, EntryKind::File | EntryKind::Directory)
            && results.hard_link_to(id).is_none()
        {
            allocated(&results.to_path_buf(id))
        } else {
            0
        };
        let children: u64 = results
            .child_ids(id)
            .map(|child| results.bytes(child))
            .sum();
        assert_eq!(
            results.bytes(id),
            own + children,
            "{}",
            results.to_path_buf(id).display()
        );
    }
}

type Hook = Arc<dyn Fn(&Path) -> io::Result<()> + Send + Sync>;
type MetadataHook = Arc<dyn Fn(&Path, EntryMetadata) -> EntryMetadata + Send + Sync>;

struct TestFileSystem {
    real: RootFileSystem,
    root: PathBuf,
    metadata_override: MetadataHook,
    before_read: Hook,
    before_metadata: Hook,
    before_next: Hook,
    broken_listing: bool,
}

impl TestFileSystem {
    fn new(root: &Path) -> Self {
        Self {
            real: RootFileSystem::open(root).unwrap(),
            root: root.into(),
            metadata_override: Arc::new(|_, metadata| metadata),
            before_read: Arc::new(|_| Ok(())),
            before_metadata: Arc::new(|_| Ok(())),
            before_next: Arc::new(|_| Ok(())),
            broken_listing: false,
        }
    }
}

impl TestFileSystem {
    fn wrap(&self, inner: Box<dyn Directory>, path: PathBuf) -> Box<dyn Directory> {
        Box::new(TestDirectory {
            inner,
            path,
            before_read: self.before_read.clone(),
            before_metadata: self.before_metadata.clone(),
            metadata_override: self.metadata_override.clone(),
            before_next: self.before_next.clone(),
            broken_listing: self.broken_listing,
        })
    }
}

impl FileSystem for TestFileSystem {
    fn root_metadata(&self) -> EntryMetadata {
        (self.metadata_override)(&self.root, self.real.root_metadata())
    }

    fn open_root(&self) -> io::Result<Box<dyn Directory>> {
        (self.before_read)(&self.root)?;
        Ok(self.wrap(self.real.open_root()?, self.root.clone()))
    }
}

struct TestDirectory {
    inner: Box<dyn Directory>,
    metadata_override: MetadataHook,
    path: PathBuf,
    before_read: Hook,
    before_metadata: Hook,
    before_next: Hook,
    broken_listing: bool,
}

impl Directory for TestDirectory {
    fn entries(&self) -> io::Result<Box<dyn Iterator<Item = EntryItem> + '_>> {
        Ok(Box::new(TestEntries {
            inner: self.inner.entries()?,
            metadata_override: self.metadata_override.clone(),
            path: self.path.clone(),
            before_metadata: self.before_metadata.clone(),
            before_next: self.before_next.clone(),
            broken_listing: self.broken_listing,
        }))
    }

    fn open_child(&self, name: &OsStr, expected: FileIdentity) -> io::Result<Box<dyn Directory>> {
        let path = self.path.join(name);
        (self.before_read)(&path)?;
        Ok(Box::new(TestDirectory {
            inner: self.inner.open_child(name, expected)?,
            path,
            before_read: self.before_read.clone(),
            metadata_override: self.metadata_override.clone(),
            before_metadata: self.before_metadata.clone(),
            before_next: self.before_next.clone(),
            broken_listing: self.broken_listing,
        }))
    }
}

struct TestEntries<'a> {
    inner: Box<dyn Iterator<Item = EntryItem> + 'a>,
    metadata_override: MetadataHook,
    path: PathBuf,
    before_metadata: Hook,
    before_next: Hook,
    broken_listing: bool,
}

impl Iterator for TestEntries<'_> {
    type Item = EntryItem;

    fn next(&mut self) -> Option<Self::Item> {
        if let Err(error) = (self.before_next)(&self.path) {
            return Some(EntryItem::Error(EntryError { name: None, error }));
        }
        match self.inner.next() {
            Some(EntryItem::Entry(mut entry)) => {
                let full = self.path.join(&entry.name);
                if let Err(error) = (self.before_metadata)(&full) {
                    return Some(EntryItem::Error(EntryError {
                        name: Some(entry.name),
                        error,
                    }));
                }
                entry.metadata = (self.metadata_override)(&full, entry.metadata);
                Some(EntryItem::Entry(entry))
            }
            Some(item) => Some(item),
            None => std::mem::take(&mut self.broken_listing).then(|| {
                EntryItem::Error(EntryError {
                    name: None,
                    error: io::Error::other("directory listing interrupted"),
                })
            }),
        }
    }
}

fn denied(root: &Path, blocked: PathBuf) -> TestFileSystem {
    TestFileSystem {
        before_read: Arc::new(move |path| {
            if path == blocked {
                Err(io::ErrorKind::PermissionDenied.into())
            } else {
                Ok(())
            }
        }),
        ..TestFileSystem::new(root)
    }
}

#[test]
fn unreadable_directory_is_reported_separately_and_is_never_empty() {
    let temp = TempDirectory::new();
    temp.file("readable", 25);
    temp.file("blocked/hidden", 100);
    let blocked = temp.0.join("blocked");
    let results = collect(&temp.0, &denied(&temp.0, blocked.clone()));
    assert_eq!(results.state, ScanState::Partial);
    assert_allocated_totals(&results);
    assert_eq!(results.issues.len(), 1);
    assert_eq!(results.issues[0].path, blocked);
    assert!(!node(&results, &blocked).is_complete());
    assert_eq!(node(&results, &blocked).errors(), 1);
    assert!(!results.is_complete(0));
}

#[test]
fn root_read_failure_and_invalid_roots_fail_explicitly() {
    let temp = TempDirectory::new();
    let file = temp.file("file", 9);
    for root in [file, temp.0.join("missing")] {
        let results = collect_real(&root);
        assert_eq!(results.state, ScanState::Failed);
        assert!(results.is_empty());
        assert_eq!(results.issues.len(), 1);
        assert_eq!(results.issues[0].path, root);
    }
    let results = collect(&temp.0, &denied(&temp.0, temp.0.clone()));
    assert_eq!(results.state, ScanState::Failed);
    assert_eq!(results.issues.len(), 1);
    assert!(!results.is_complete(0));
}

fn changing_file(root: &Path, target: PathBuf, remove: bool) -> TestFileSystem {
    TestFileSystem {
        before_metadata: Arc::new(move |path| {
            if path == target {
                if remove {
                    fs::remove_file(path)?;
                    return Err(io::ErrorKind::NotFound.into());
                } else {
                    fs::write(path, [0; 80 * 1024])?;
                }
            }
            Ok(())
        }),
        ..TestFileSystem::new(root)
    }
}

#[test]
fn a_file_removed_during_scan_is_an_issue_not_an_empty_success() {
    let temp = TempDirectory::new();
    let path = temp.file("disappears", 10);
    let results = collect(&temp.0, &changing_file(&temp.0, path.clone(), true));
    assert_eq!(results.state, ScanState::Partial);
    assert_eq!(results.files, 0);
    assert_eq!(results.bytes(0), allocated(&temp.0));
    assert_eq!(results.issues.len(), 1);
    assert_eq!(results.issues[0].path, path);
    assert!(!results.is_complete(0));
}

#[test]
fn file_allocation_is_observed_when_the_directory_is_listed() {
    let temp = TempDirectory::new();
    let path = temp.file("grows", 10);
    let before = allocated(&path);
    let results = collect(&temp.0, &changing_file(&temp.0, path.clone(), false));
    assert_eq!(results.state, ScanState::Complete);
    // The bulk read supplies the allocation observed while listing; a later
    // write does not retroactively change the result.
    assert_eq!(node(&results, &path).bytes(), before);
    assert_eq!(fs::metadata(&path).unwrap().len(), 80 * 1024);
    assert_eq!(results.files, 1);
}

#[test]
fn directory_iterator_failures_keep_previously_discovered_files() {
    let temp = TempDirectory::new();
    temp.file("known", 30);
    let results = collect(
        &temp.0,
        &TestFileSystem {
            broken_listing: true,
            ..TestFileSystem::new(&temp.0)
        },
    );
    assert_eq!(results.state, ScanState::Partial);
    assert_eq!(results.files, 1);
    assert_allocated_totals(&results);
    assert_eq!(results.issues.len(), 1);
    assert!(!results.is_complete(0));
}

#[test]
fn worker_delivers_bounded_batches_and_flushes_on_cancellation() {
    let temp = TempDirectory::new();
    for index in 0..1600 {
        temp.file(&index.to_string(), 1);
    }
    let job = ScanJob::start(temp.0.clone()).unwrap();
    let mut results = ScanResults::scanning();
    let first = job.updates.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(!first.is_empty() && first.len() <= 128);
    job.cancel();
    for event in first {
        results.apply(event);
    }
    while results.state == ScanState::Scanning {
        let batch = job.updates.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(!batch.is_empty() && batch.len() <= 128);
        for event in batch {
            results.apply(event);
        }
    }
    assert_eq!(results.state, ScanState::Partial);
    assert!(results.files > 0 && results.files < 1600);
    assert_allocated_totals(&results);
    assert!(!results.is_complete(0));
}

#[test]
fn queued_directory_replacements_are_incomplete_and_never_read() {
    if !extended_ids_supported() {
        return;
    }
    for replace_with_symlink in [false, true] {
        if replace_with_symlink && !symlinks_supported() {
            continue;
        }
        let temp = TempDirectory::new();
        let external = TempDirectory::new();
        let backup = TempDirectory::new();
        temp.file("known", 17);
        temp.file("queued/original", 40);
        external.file("outside", 999);
        let expected = allocated(&temp.0)
            + allocated(&temp.0.join("known"))
            + allocated(&temp.0.join("queued"));
        let target = temp.0.join("queued");
        let moved = backup.0.join("original");
        let outside = external.0.clone();
        let fs = TestFileSystem {
            before_read: Arc::new(move |path| {
                if path == target {
                    fs::rename(path, &moved)?;
                    if replace_with_symlink {
                        symlink_dir(&outside, path)?;
                    } else {
                        fs::create_dir(path)?;
                        fs::write(path.join("replacement"), [0; 50])?;
                    }
                }
                Ok(())
            }),
            ..TestFileSystem::new(&temp.0)
        };
        let results = collect(&temp.0, &fs);
        assert_eq!(results.state, ScanState::Partial);
        assert_eq!(results.files, 1);
        assert_eq!(results.bytes(0), expected);
        assert_eq!(results.issues.len(), 1);
        assert!(!node(&results, &temp.0.join("queued")).is_complete());
        assert!(node(&results, &temp.0.join("queued")).children().is_empty());
    }
}

#[test]
fn replacing_a_queued_ancestor_with_a_symlink_cannot_escape_the_root() {
    if !symlinks_supported() {
        return;
    }
    let temp = TempDirectory::new();
    let external = TempDirectory::new();
    let backup = TempDirectory::new();
    temp.file("outer/inner/original", 20);
    external.file("inner/outside", 999);
    let file = temp.0.join("outer/inner/original");
    let expected = allocated(&temp.0)
        + allocated(&temp.0.join("outer"))
        + allocated(&temp.0.join("outer/inner"))
        + allocated(&file);
    let outer = temp.0.join("outer");
    let inner = outer.join("inner");
    let moved = backup.0.join("original");
    let outside = external.0.clone();
    let fs = TestFileSystem {
        before_read: Arc::new(move |path| {
            if path == inner {
                fs::rename(&outer, &moved)?;
                symlink_dir(&outside, &outer)?;
            }
            Ok(())
        }),
        ..TestFileSystem::new(&temp.0)
    };
    let results = collect(&temp.0, &fs);
    // Traversal is anchored to the already open `outer` handle, so replacing
    // its path with a symlink neither escapes the root nor interrupts the
    // scan: the original directory is still read.
    assert_eq!(results.state, ScanState::Complete);
    assert_eq!(results.files, 1);
    assert_eq!(results.bytes(0), expected);
    assert!(results.issues.is_empty());
    assert!(node(&results, &temp.0.join("outer/inner")).is_complete());
}

#[test]
fn metadata_stays_relative_to_the_open_directory_after_path_replacement() {
    if !symlinks_supported() {
        return;
    }
    let temp = TempDirectory::new();
    let external = TempDirectory::new();
    let backup = TempDirectory::new();
    let original = temp.file("directory/file", 20);
    let expected = allocated(&temp.0) + allocated(&temp.0.join("directory")) + allocated(&original);
    external.file("file", 999);
    let directory = temp.0.join("directory");
    let moved = backup.0.join("original");
    let outside = external.0.clone();
    let fs = TestFileSystem {
        before_metadata: Arc::new(move |path| {
            if path == original {
                fs::rename(&directory, &moved)?;
                symlink_dir(&outside, &directory)?;
            }
            Ok(())
        }),
        ..TestFileSystem::new(&temp.0)
    };
    let results = collect(&temp.0, &fs);
    assert_eq!(results.state, ScanState::Complete);
    assert_eq!(results.files, 1);
    assert_eq!(results.bytes(0), expected);
}

#[test]
fn the_selected_root_is_anchored_even_if_its_path_is_replaced() {
    if !symlinks_supported() {
        return;
    }
    let temp = TempDirectory::new();
    let external = TempDirectory::new();
    let backup = TempDirectory::new();
    temp.file("nested/known", 20);
    let expected = allocated(&temp.0)
        + allocated(&temp.0.join("nested"))
        + allocated(&temp.0.join("nested/known"));
    external.file("outside", 999);
    let root = temp.0.clone();
    let moved = backup.0.join("original");
    let outside = external.0.clone();
    let fs = TestFileSystem {
        before_read: Arc::new(move |path| {
            if path == root {
                fs::rename(path, &moved)?;
                symlink_dir(&outside, path)?;
            }
            Ok(())
        }),
        ..TestFileSystem::new(&temp.0)
    };
    let results = collect(&temp.0, &fs);
    assert_eq!(results.state, ScanState::Complete);
    assert_eq!(results.files, 1);
    assert_eq!(results.bytes(0), expected);
}

#[test]
fn symlink_roots_with_trailing_separator_or_dot_are_rejected() {
    if !symlinks_supported() {
        return;
    }
    let temp = TempDirectory::new();
    let external = TempDirectory::new();
    symlink_dir(&external.0, &temp.0.join("link")).unwrap();
    for suffix in ["link/", "link/."] {
        let results = collect_real(&temp.0.join(suffix));
        assert_eq!(results.state, ScanState::Failed);
        assert!(results.is_empty());
    }
}

#[test]
fn cancellation_during_iteration_does_not_read_more_metadata_or_mark_empty() {
    let temp = TempDirectory::new();
    temp.file("file", 20);
    let cancel = Arc::new(AtomicBool::new(false));
    let iter_cancel = cancel.clone();
    let fs = TestFileSystem {
        before_next: Arc::new(move |_| {
            iter_cancel.store(true, Ordering::Relaxed);
            Ok(())
        }),
        ..TestFileSystem::new(&temp.0)
    };
    let mut results = ScanResults::default();
    scan(&temp.0, &cancel, &fs, &mut |event| {
        results.apply(event);
        true
    });
    assert_eq!(results.state, ScanState::Partial);
    assert_eq!(results.files, 0);
    assert!(!results.is_complete(0));
}

#[test]
fn cancellation_before_start_does_not_touch_the_filesystem() {
    let mut results = ScanResults::default();
    scan_real(
        Path::new("/missing/clean-test-root"),
        &AtomicBool::new(true),
        &mut |event| {
            results.apply(event);
            true
        },
    );
    assert_eq!(results.state, ScanState::Partial);
    assert!(results.is_empty());
    assert!(results.issues.is_empty());
}

#[test]
fn receiver_disconnection_stops_traversal_immediately() {
    let temp = TempDirectory::new();
    let fs = TestFileSystem {
        before_read: Arc::new(|_| panic!("no directory should be read after receiver disconnects")),
        ..TestFileSystem::new(&temp.0)
    };
    let mut events = 0;
    scan(&temp.0, &AtomicBool::new(false), &fs, &mut |_| {
        events += 1;
        false
    });
    assert_eq!(events, 1);
}

#[test]
fn sparse_files_keep_only_written_blocks_and_their_exact_path() {
    let temp = TempDirectory::new();
    // Windows forbids control characters in names; macOS allows the newline.
    #[cfg(windows)]
    let path = temp.0.join("Unicode 空格 'quotes'.bin");
    #[cfg(not(windows))]
    let path = temp.0.join("Unicode 空格 'quotes'\n.bin");
    use std::io::{Seek, SeekFrom, Write};
    let mut file = fs::File::create(&path).unwrap();
    if !mark_sparse(&file) {
        return;
    }
    file.set_len(5 * 1024 * 1024 * 1024).unwrap();
    file.write_all(&[1; 4096]).unwrap();
    file.seek(SeekFrom::End(-4096)).unwrap();
    file.write_all(&[2; 4096]).unwrap();
    let results = collect_real(&temp.0);
    assert_eq!(results.state, ScanState::Complete);
    assert_eq!(results.files, 1);
    assert_allocated_totals(&results);
    assert!(node(&results, &path).bytes() > 0);
    assert!(node(&results, &path).bytes() < fs::metadata(&path).unwrap().len());
}

#[test]
fn bounded_sender_unblocks_when_receiver_is_dropped() {
    let (sender, receiver) = mpsc::sync_channel(1);
    let (blocked, ready) = mpsc::channel();
    let (finished, done) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut batches = Batches::new(sender);
        batches.last_flush = Instant::now() + Duration::from_secs(3600);
        for _ in 0..128 {
            assert!(batches.push(ScanEvent::DirectoryRead(0)));
        }
        blocked.send(()).unwrap();
        let mut connected = true;
        for _ in 0..128 {
            connected &= batches.push(ScanEvent::DirectoryRead(0));
        }
        finished.send(connected).unwrap();
    });
    ready.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty)));
    drop(receiver);
    assert!(!done.recv_timeout(Duration::from_secs(10)).unwrap());
    worker.join().unwrap();
}

#[test]
fn deep_directory_traversal_does_not_hold_a_descriptor_for_each_ancestor() {
    let temp = TempDirectory::new();
    let relative = format!("{}file", "d/".repeat(160));
    temp.file(&relative, 23);
    let results = collect_real(&temp.0);
    assert_eq!(results.state, ScanState::Complete);
    assert_eq!(results.directories, 161);
    assert_eq!(results.files, 1);
    assert_allocated_totals(&results);
    assert!((0..results.len()).all(|id| results.is_complete(id)));
}

#[test]
#[ignore = "manual filesystem throughput test; creates 20,000 temporary sparse files"]
fn large_directory_worker_pipeline() {
    use crate::analysis::{DirectoryIndex, LargeFileIndex};
    const FILES: usize = 20_000;
    let temp = TempDirectory::new();
    for id in 0..FILES {
        fs::File::create(temp.0.join(format!("file-{id:05}")))
            .unwrap()
            .set_len((id % 8192) as u64)
            .unwrap();
    }
    let mut results = ScanResults::scanning();
    let mut directory = DirectoryIndex::default();
    directory.select(&results, 0);
    let mut files = LargeFileIndex::default();
    let started = Instant::now();
    let job = ScanJob::start(temp.0.clone()).unwrap();
    let mut batches = 0;
    while results.state == ScanState::Scanning {
        let batch = job.updates.recv_timeout(Duration::from_secs(30)).unwrap();
        assert!(!batch.is_empty() && batch.len() <= 128);
        let changes = results.apply_batch(batch);
        directory.update(&results, &changes);
        files.update(&results);
        batches += 1;
    }
    let elapsed = started.elapsed();
    assert_eq!(results.state, ScanState::Complete);
    assert_eq!(results.files, FILES);
    assert_allocated_totals(&results);
    assert!((0..results.len()).all(|id| results.is_complete(id)));
    assert!(results.issues.is_empty());
    assert_eq!(directory.sorted(&results), sorted_children(&results, 0));
    assert_eq!(files.matching(&results, 0).len(), FILES);
    println!(
        "{FILES} files, {batches} bounded batches, scan + aggregation + indexes: {elapsed:?} ({:.0} files/s)",
        FILES as f64 / elapsed.as_secs_f64()
    );
}

#[test]
fn a_terabyte_sparse_image_is_charged_only_its_allocated_blocks() {
    let temp = TempDirectory::new();
    let image = temp.0.join("virtual-disk.raw");
    let logical_bytes = 1024_u64.pow(4);
    // set_len creates holes; this test does not write or allocate a TiB of data.
    let image_file = fs::File::create(&image).unwrap();
    if !mark_sparse(&image_file) {
        return;
    }
    image_file.set_len(logical_bytes).unwrap();
    let metadata = fs::symlink_metadata(&image).unwrap();
    let allocated_bytes = allocated(&image);
    let results = collect_real(&temp.0);
    assert_eq!(results.state, ScanState::Complete);
    assert_eq!(results.files, 1);
    assert_eq!(results.len(), 2);
    assert_eq!(node(&results, &image).bytes(), allocated_bytes);
    assert_allocated_totals(&results);
    assert!(metadata.len() > 512_000_000_000);
    assert!(results.bytes(0) < 512_000_000_000);
    assert!(allocated_bytes < logical_bytes);
    assert_eq!(link_count(&image), 1);
    assert!(results.issues.is_empty());
}

#[test]
fn directory_own_allocation_is_included_even_when_it_has_no_file_bytes() {
    let temp = TempDirectory::new();
    let file = temp.file("child/file", 3);
    fs::create_dir(temp.0.join("empty")).unwrap();
    let root = temp.0.clone();
    let fs = TestFileSystem {
        metadata_override: Arc::new(move |path, mut metadata| {
            if metadata.kind == EntryKind::Directory {
                metadata.allocated_bytes = if path == root { 1024 } else { 512 };
            }
            metadata
        }),
        ..TestFileSystem::new(&temp.0)
    };
    let results = collect(&temp.0, &fs);
    assert_eq!(results.state, ScanState::Complete);
    assert_eq!(results.bytes(0), 1024 + 512 + 512 + allocated(&file));
    assert_eq!(
        node(&results, &temp.0.join("child")).bytes(),
        512 + allocated(&file)
    );
    assert_eq!(node(&results, &temp.0.join("empty")).bytes(), 512);
    assert!((0..results.len()).all(|id| results.is_complete(id)));
}

#[test]
fn matching_inodes_on_different_devices_are_not_deduplicated() {
    let temp = TempDirectory::new();
    let a = temp.file("a", 11);
    let b = temp.file("b", 11);
    let fs = TestFileSystem {
        metadata_override: Arc::new(|path, mut metadata| {
            if metadata.kind == EntryKind::File {
                metadata.identity = FileIdentity::new(
                    if path.file_name().unwrap() == "a" {
                        1
                    } else {
                        2
                    },
                    42,
                );
            }
            metadata
        }),
        ..TestFileSystem::new(&temp.0)
    };
    let results = collect(&temp.0, &fs);
    assert_eq!(results.state, ScanState::Complete);
    assert_eq!(results.files, 2);
    assert_eq!(results.hard_links, 0);
    assert_eq!(
        results.bytes(0),
        allocated(&temp.0) + allocated(&a) + allocated(&b)
    );
    assert_allocated_totals(&results);
}

#[test]
fn cancelling_after_a_hard_link_preserves_only_one_allocation() {
    if !extended_ids_supported() {
        return;
    }
    let temp = TempDirectory::new();
    let original = temp.file("original", 37);
    fs::hard_link(&original, temp.0.join("alias")).unwrap();
    fs::create_dir(temp.0.join("nested")).unwrap();
    fs::hard_link(&original, temp.0.join("nested/alias")).unwrap();
    let cancel = AtomicBool::new(false);
    let mut results = ScanResults::default();
    scan(
        &temp.0,
        &cancel,
        &RootFileSystem::open(&temp.0).unwrap(),
        &mut |event| {
            results.apply(event);
            if results.files == 2 {
                cancel.store(true, Ordering::Relaxed);
            }
            true
        },
    );
    assert_eq!(results.state, ScanState::Partial);
    assert_eq!(results.files, 2);
    assert_eq!(results.hard_links, 1);
    assert_allocated_totals(&results);
    assert!(!results.is_complete(0));
    let rescanned = collect_real(&temp.0);
    assert_eq!(rescanned.state, ScanState::Complete);
    assert_eq!(rescanned.files, 3);
    assert_eq!(rescanned.hard_links, 2);
    assert_allocated_totals(&rescanned);
}

#[test]
fn hard_link_deduplication_survives_worker_batch_boundaries() {
    if !extended_ids_supported() {
        return;
    }
    use crate::analysis::{DirectoryIndex, LargeFileIndex};
    let temp = TempDirectory::new();
    let original = temp.file("original", 37);
    for id in 0..300 {
        fs::hard_link(&original, temp.0.join(format!("alias-{id}"))).unwrap();
    }
    let job = ScanJob::start(temp.0.clone()).unwrap();
    let mut results = ScanResults::scanning();
    let mut files = LargeFileIndex::default();
    let mut directory = DirectoryIndex::default();
    directory.select(&results, 0);
    let mut batches = 0;
    while results.state == ScanState::Scanning {
        let batch = job.updates.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(!batch.is_empty() && batch.len() <= 128);
        let changes = results.apply_batch(batch);
        files.update(&results);
        directory.update(&results, &changes);
        batches += 1;
    }
    assert!(batches >= 3);
    assert_eq!(results.state, ScanState::Complete);
    assert_eq!(results.files, 301);
    assert_eq!(results.hard_links, 300);
    assert_eq!(results.bytes(0), allocated(&temp.0) + allocated(&original));
    assert_allocated_totals(&results);
    let owners = files.matching(&results, 0);
    assert_eq!(owners.len(), 1);
    for id in 0..results.len() {
        if let Some(owner) = results.hard_link_to(id) {
            assert_eq!(owner, owners[0]);
        }
    }
    assert_eq!(directory.sorted(&results).len(), 301);
    assert_eq!(directory.sorted(&results), sorted_children(&results, 0));
}

#[test]
#[ignore]
fn benchmark_real_tree() {
    use std::time::Instant;
    let root = std::env::var_os("CLEAN_BENCH_ROOT")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .unwrap();
    let rounds: u32 = std::env::var("CLEAN_BENCH_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let apply = std::env::var_os("CLEAN_BENCH_APPLY").is_some();
    for round in 0..rounds {
        let mut entries = 0usize;
        let mut bytes = 0u64;
        let mut errors = 0usize;
        let start = Instant::now();
        scan_real(&root, &AtomicBool::new(false), &mut |event| {
            match &event {
                ScanEvent::Entry { bytes: b, .. } => {
                    entries += 1;
                    bytes += b;
                }
                ScanEvent::Issue { .. } => errors += 1,
                _ => {}
            }
            true
        });
        let scan_ms = start.elapsed().as_secs_f64() * 1000.0;
        let start = Instant::now();
        let (nodes, state) = if apply {
            let results = collect_real(&root);
            (results.len(), format!("{:?}", results.state))
        } else {
            (0, "skipped".into())
        };
        let apply_ms = start.elapsed().as_secs_f64() * 1000.0;
        println!(
            "round {round}: scan {scan_ms:.1} ms ({:.0} entries/s), scan+apply {apply_ms:.1} ms, entries {entries}, nodes {nodes}, bytes {bytes}, issues {errors}, state {state}",
            entries as f64 / (scan_ms / 1000.0),
        );
    }
}
