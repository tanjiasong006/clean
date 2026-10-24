use std::{
    ffi::{OsStr, OsString},
    fs,
    rc::Rc,
    sync::atomic::AtomicUsize,
};

use super::*;
use crate::{
    analysis::sorted_children,
    models::ScanResults,
    platform::scan_fs::{DirectoryReader, EntryMetadata, FileIdentity},
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
    let mut results = ScanResults {
        state: ScanState::Scanning,
        ..Default::default()
    };
    scan_real(root, &AtomicBool::new(false), &mut |event| {
        results.apply(event);
        true
    });
    results
}

fn collect(root: &Path, fs: &dyn FileSystem) -> ScanResults {
    let mut results = ScanResults {
        state: ScanState::Scanning,
        ..Default::default()
    };
    scan(root, &AtomicBool::new(false), fs, &mut |event| {
        results.apply(event);
        true
    });
    results
}

fn node<'a>(results: &'a ScanResults, path: &Path) -> &'a crate::models::Node {
    results
        .nodes
        .iter()
        .find(|node| node.path.as_ref() == path)
        .unwrap()
}

#[test]
fn aggregates_nested_directories_and_sorts_by_logical_size() {
    let temp = TempDirectory::new();
    temp.file("small", 8);
    temp.file("nested/a", 40);
    temp.file("nested/deep/b", 60);
    fs::create_dir(temp.0.join("empty")).unwrap();
    let results = collect_real(&temp.0);
    assert_eq!(results.state, ScanState::Complete);
    assert_eq!(results.files, 3);
    assert_eq!(results.directories, 4); // Includes the selected root.
    assert_eq!(results.nodes[0].bytes, 108);
    assert_eq!(node(&results, &temp.0.join("nested")).bytes, 100);
    assert_eq!(node(&results, &temp.0.join("nested/deep")).bytes, 60);
    assert!(results.nodes.iter().all(|node| node.is_complete()));
    let children = sorted_children(&results, 0);
    assert_eq!(
        results.nodes[children[0]].path.as_ref(),
        &temp.0.join("nested")
    );
    assert_eq!(
        results.nodes[children[1]].path.as_ref(),
        &temp.0.join("small")
    );
    assert_eq!(
        results.nodes[children[2]].path.as_ref(),
        &temp.0.join("empty")
    );
    assert!(results.issues.is_empty());
}

#[cfg(unix)]
#[test]
fn skips_directory_file_dangling_and_cyclic_symlinks() {
    use std::os::unix::fs::symlink;
    let temp = TempDirectory::new();
    let external = TempDirectory::new();
    external.file("large", 500);
    let file = temp.file("file", 12);
    symlink(&external.0, temp.0.join("external")).unwrap();
    symlink(&temp.0, temp.0.join("cycle")).unwrap();
    symlink(&file, temp.0.join("link")).unwrap();
    symlink(temp.0.join("missing"), temp.0.join("dangling")).unwrap();
    let results = collect_real(&temp.0);
    assert_eq!(results.state, ScanState::Complete);
    assert_eq!(results.nodes[0].bytes, 12);
    assert_eq!(results.files, 1);
    assert_eq!(results.directories, 1);
    assert_eq!(results.symlinks, 4);
    assert_eq!(results.nodes.len(), 6);
    assert!(results.issues.is_empty());
    let linked_root = collect_real(&temp.0.join("external"));
    assert_eq!(linked_root.state, ScanState::Failed);
    assert!(linked_root.nodes.is_empty());
}

#[test]
fn hard_links_are_counted_once_per_path_even_across_directories() {
    let temp = TempDirectory::new();
    let original = temp.file("original", 37);
    fs::create_dir(temp.0.join("nested")).unwrap();
    fs::hard_link(&original, temp.0.join("same-level")).unwrap();
    fs::hard_link(&original, temp.0.join("nested/linked")).unwrap();
    let results = collect_real(&temp.0);
    assert_eq!(results.state, ScanState::Complete);
    assert_eq!(results.files, 3);
    assert_eq!(results.nodes[0].bytes, 111);
    assert_eq!(node(&results, &temp.0.join("nested")).bytes, 37);
}

#[test]
fn cancellation_keeps_partial_results_and_unread_directories_incomplete() {
    let temp = TempDirectory::new();
    for index in 0..10 {
        temp.file(&format!("nested/{index}"), 10);
    }
    let cancel = AtomicBool::new(false);
    let mut results = ScanResults {
        state: ScanState::Scanning,
        ..Default::default()
    };
    scan(
        &temp.0,
        &cancel,
        &RootFileSystem::open(&temp.0).unwrap(),
        &mut |event| {
            results.apply(event);
            if results.files == 1 {
                cancel.store(true, Ordering::Relaxed);
            }
            true
        },
    );
    assert_eq!(results.state, ScanState::Partial);
    assert_eq!(results.files, 1);
    assert_eq!(results.nodes[0].bytes, 10);
    assert!(!results.nodes[0].is_complete());
    assert!(!node(&results, &temp.0.join("nested")).is_complete());
    assert!(results.issues.is_empty());
}

type Hook = Rc<dyn Fn(&Path) -> io::Result<()>>;

struct TestFileSystem {
    real: RootFileSystem,
    before_read: Hook,
    before_metadata: Hook,
    before_next: Hook,
    broken_listing: bool,
}

impl TestFileSystem {
    fn new(root: &Path) -> Self {
        Self {
            real: RootFileSystem::open(root).unwrap(),
            before_read: Rc::new(|_| Ok(())),
            before_metadata: Rc::new(|_| Ok(())),
            before_next: Rc::new(|_| Ok(())),
            broken_listing: false,
        }
    }
}

impl FileSystem for TestFileSystem {
    fn root_metadata(&self) -> EntryMetadata {
        self.real.root_metadata()
    }

    fn read_dir(
        &self,
        path: &Path,
        expected: FileIdentity,
    ) -> io::Result<Box<dyn DirectoryReader>> {
        (self.before_read)(path)?;
        Ok(Box::new(TestReader {
            inner: self.real.read_dir(path, expected)?,
            path: path.into(),
            before_metadata: self.before_metadata.clone(),
            before_next: self.before_next.clone(),
            broken_listing: self.broken_listing,
        }))
    }
}

struct TestReader {
    inner: Box<dyn DirectoryReader>,
    path: PathBuf,
    before_metadata: Hook,
    before_next: Hook,
    broken_listing: bool,
}

impl Iterator for TestReader {
    type Item = io::Result<OsString>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Err(error) = (self.before_next)(&self.path) {
            return Some(Err(error));
        }
        self.inner.next().or_else(|| {
            std::mem::take(&mut self.broken_listing)
                .then(|| Err(io::Error::other("directory listing interrupted")))
        })
    }
}

impl DirectoryReader for TestReader {
    fn metadata(&self, name: &OsStr) -> io::Result<EntryMetadata> {
        (self.before_metadata)(&self.path.join(name))?;
        self.inner.metadata(name)
    }
}

fn denied(root: &Path, blocked: PathBuf) -> TestFileSystem {
    TestFileSystem {
        before_read: Rc::new(move |path| {
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
    assert_eq!(results.nodes[0].bytes, 25);
    assert_eq!(results.issues.len(), 1);
    assert_eq!(results.issues[0].path, blocked);
    assert!(!node(&results, &blocked).is_complete());
    assert_eq!(node(&results, &blocked).errors, 1);
    assert!(!results.nodes[0].is_complete());
}

#[test]
fn root_read_failure_and_invalid_roots_fail_explicitly() {
    let temp = TempDirectory::new();
    let file = temp.file("file", 9);
    for root in [file, temp.0.join("missing")] {
        let results = collect_real(&root);
        assert_eq!(results.state, ScanState::Failed);
        assert!(results.nodes.is_empty());
        assert_eq!(results.issues.len(), 1);
        assert_eq!(results.issues[0].path, root);
    }
    let results = collect(&temp.0, &denied(&temp.0, temp.0.clone()));
    assert_eq!(results.state, ScanState::Failed);
    assert_eq!(results.issues.len(), 1);
    assert!(!results.nodes[0].is_complete());
}

fn changing_file(root: &Path, target: PathBuf, remove: bool) -> TestFileSystem {
    TestFileSystem {
        before_metadata: Rc::new(move |path| {
            if path == target {
                if remove {
                    fs::remove_file(path)?;
                } else {
                    fs::write(path, [0; 80])?;
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
    assert_eq!(results.nodes[0].bytes, 0);
    assert_eq!(results.issues.len(), 1);
    assert_eq!(results.issues[0].path, path);
    assert!(!results.nodes[0].is_complete());
}

#[test]
fn file_size_is_the_logical_size_observed_at_metadata_read_time() {
    let temp = TempDirectory::new();
    let path = temp.file("grows", 10);
    let results = collect(&temp.0, &changing_file(&temp.0, path, false));
    assert_eq!(results.state, ScanState::Complete);
    assert_eq!(results.nodes[0].bytes, 80);
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
    assert_eq!(results.nodes[0].bytes, 30);
    assert_eq!(results.issues.len(), 1);
    assert!(!results.nodes[0].is_complete());
}

#[test]
fn worker_delivers_bounded_batches_and_flushes_on_cancellation() {
    let temp = TempDirectory::new();
    for index in 0..1600 {
        temp.file(&index.to_string(), 1);
    }
    let job = ScanJob::start(temp.0.clone()).unwrap();
    let mut results = ScanResults {
        state: ScanState::Scanning,
        ..Default::default()
    };
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
    assert_eq!(results.nodes[0].bytes, results.files as u64);
    assert!(!results.nodes[0].is_complete());
}

#[test]
fn queued_directory_replacements_are_incomplete_and_never_read() {
    use std::os::unix::fs::symlink;
    for replace_with_symlink in [false, true] {
        let temp = TempDirectory::new();
        let external = TempDirectory::new();
        let backup = TempDirectory::new();
        temp.file("known", 17);
        temp.file("queued/original", 40);
        external.file("outside", 999);
        let target = temp.0.join("queued");
        let moved = backup.0.join("original");
        let outside = external.0.clone();
        let fs = TestFileSystem {
            before_read: Rc::new(move |path| {
                if path == target {
                    fs::rename(path, &moved)?;
                    if replace_with_symlink {
                        symlink(&outside, path)?;
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
        assert_eq!(results.nodes[0].bytes, 17);
        assert_eq!(results.issues.len(), 1);
        assert!(!node(&results, &temp.0.join("queued")).is_complete());
        assert!(node(&results, &temp.0.join("queued")).children.is_empty());
    }
}

#[test]
fn replacing_a_queued_ancestor_with_a_symlink_cannot_escape_the_root() {
    use std::os::unix::fs::symlink;
    let temp = TempDirectory::new();
    let external = TempDirectory::new();
    let backup = TempDirectory::new();
    temp.file("outer/inner/original", 20);
    external.file("inner/outside", 999);
    let outer = temp.0.join("outer");
    let inner = outer.join("inner");
    let moved = backup.0.join("original");
    let outside = external.0.clone();
    let fs = TestFileSystem {
        before_read: Rc::new(move |path| {
            if path == inner {
                fs::rename(&outer, &moved)?;
                symlink(&outside, &outer)?;
            }
            Ok(())
        }),
        ..TestFileSystem::new(&temp.0)
    };
    let results = collect(&temp.0, &fs);
    assert_eq!(results.state, ScanState::Partial);
    assert_eq!(results.files, 0);
    assert_eq!(results.nodes[0].bytes, 0);
    assert_eq!(results.issues.len(), 1);
    assert!(!node(&results, &temp.0.join("outer/inner")).is_complete());
}

#[test]
fn metadata_stays_relative_to_the_open_directory_after_path_replacement() {
    use std::os::unix::fs::symlink;
    let temp = TempDirectory::new();
    let external = TempDirectory::new();
    let backup = TempDirectory::new();
    let original = temp.file("directory/file", 20);
    external.file("file", 999);
    let directory = temp.0.join("directory");
    let moved = backup.0.join("original");
    let outside = external.0.clone();
    let fs = TestFileSystem {
        before_metadata: Rc::new(move |path| {
            if path == original {
                fs::rename(&directory, &moved)?;
                symlink(&outside, &directory)?;
            }
            Ok(())
        }),
        ..TestFileSystem::new(&temp.0)
    };
    let results = collect(&temp.0, &fs);
    assert_eq!(results.state, ScanState::Complete);
    assert_eq!(results.files, 1);
    assert_eq!(results.nodes[0].bytes, 20);
}

#[test]
fn the_selected_root_is_anchored_even_if_its_path_is_replaced() {
    use std::os::unix::fs::symlink;
    let temp = TempDirectory::new();
    let external = TempDirectory::new();
    let backup = TempDirectory::new();
    temp.file("nested/known", 20);
    external.file("outside", 999);
    let root = temp.0.clone();
    let moved = backup.0.join("original");
    let outside = external.0.clone();
    let fs = TestFileSystem {
        before_read: Rc::new(move |path| {
            if path == root {
                fs::rename(path, &moved)?;
                symlink(&outside, path)?;
            }
            Ok(())
        }),
        ..TestFileSystem::new(&temp.0)
    };
    let results = collect(&temp.0, &fs);
    assert_eq!(results.state, ScanState::Complete);
    assert_eq!(results.files, 1);
    assert_eq!(results.nodes[0].bytes, 20);
}

#[test]
fn symlink_roots_with_trailing_separator_or_dot_are_rejected() {
    use std::os::unix::fs::symlink;
    let temp = TempDirectory::new();
    let external = TempDirectory::new();
    symlink(&external.0, temp.0.join("link")).unwrap();
    for suffix in ["link/", "link/."] {
        let results = collect_real(&temp.0.join(suffix));
        assert_eq!(results.state, ScanState::Failed);
        assert!(results.nodes.is_empty());
    }
}

#[test]
fn cancellation_during_iteration_does_not_read_more_metadata_or_mark_empty() {
    let temp = TempDirectory::new();
    temp.file("file", 20);
    let cancel = Arc::new(AtomicBool::new(false));
    let iter_cancel = cancel.clone();
    let fs = TestFileSystem {
        before_next: Rc::new(move |_| {
            iter_cancel.store(true, Ordering::Relaxed);
            Ok(())
        }),
        before_metadata: Rc::new(|_| panic!("metadata must not be read after cancellation")),
        ..TestFileSystem::new(&temp.0)
    };
    let mut results = ScanResults::default();
    scan(&temp.0, &cancel, &fs, &mut |event| {
        results.apply(event);
        true
    });
    assert_eq!(results.state, ScanState::Partial);
    assert_eq!(results.files, 0);
    assert!(!results.nodes[0].is_complete());
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
    assert!(results.nodes.is_empty());
    assert!(results.issues.is_empty());
}

#[test]
fn receiver_disconnection_stops_traversal_immediately() {
    let temp = TempDirectory::new();
    let fs = TestFileSystem {
        before_read: Rc::new(|_| panic!("no directory should be read after receiver disconnects")),
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
fn sparse_files_and_unusual_names_keep_their_logical_size_and_exact_path() {
    let temp = TempDirectory::new();
    let path = temp.0.join("Unicode 空格 'quotes'\n.bin");
    let file = fs::File::create(&path).unwrap();
    file.set_len(5 * 1024 * 1024 * 1024).unwrap();
    let results = collect_real(&temp.0);
    assert_eq!(results.state, ScanState::Complete);
    assert_eq!(results.files, 1);
    assert_eq!(node(&results, &path).bytes, 5 * 1024 * 1024 * 1024);
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
    assert_eq!(results.nodes[0].bytes, 23);
    assert!(results.nodes.iter().all(|node| node.is_complete()));
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
    let mut results = ScanResults {
        state: ScanState::Scanning,
        ..Default::default()
    };
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
    assert_eq!(
        results.nodes[0].bytes,
        (0..FILES).map(|n| (n % 8192) as u64).sum::<u64>()
    );
    assert!(results.nodes.iter().all(|node| node.is_complete()));
    assert!(results.issues.is_empty());
    assert_eq!(directory.sorted(), sorted_children(&results, 0));
    assert_eq!(files.matching(0).len(), FILES);
    println!(
        "{FILES} files, {batches} bounded batches, scan + aggregation + indexes: {elapsed:?} ({:.0} files/s)",
        FILES as f64 / elapsed.as_secs_f64()
    );
}
