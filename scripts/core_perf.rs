//! Synthetic in-memory performance check; does not measure filesystem I/O.
//! Run: rustc --edition=2024 -O scripts/core_perf.rs -o /tmp/clean-core-perf
//!      /tmp/clean-core-perf
//!      /tmp/clean-core-perf --memory 3000000
#![allow(dead_code)]

#[path = "../src/analysis/mod.rs"]
mod analysis;
#[path = "../src/models/mod.rs"]
mod models;

use std::{
    alloc::{GlobalAlloc, Layout, System},
    collections::HashMap,
    hint::black_box,
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

use analysis::{DirectoryIndex, LargeFileIndex};
use models::{EntryKind, ScanEvent, ScanResults, ScanState};

fn measure(depth: usize, indexes: bool) {
    const FILES: usize = 100_000;
    const BATCH: usize = 1024;
    let mut results = ScanResults::default();
    results.apply_batch((0..=depth).map(|id| ScanEvent::Entry {
        hard_link_to: None,
        path: format!("/root/dir-{id}").into(),
        parent: id.checked_sub(1),
        kind: EntryKind::Directory,
        bytes: 0,
    }));
    let mut files = LargeFileIndex::default();
    let mut directory = DirectoryIndex::default();
    directory.select(&results, depth);
    let started = Instant::now();
    for first in (0..FILES).step_by(BATCH) {
        let changes =
            results.apply_batch(
                (first..(first + BATCH).min(FILES)).map(|n| ScanEvent::Entry {
                    hard_link_to: None,
                    path: format!("/root/file-{n:06}").into(),
                    parent: Some(depth),
                    kind: EntryKind::File,
                    bytes: (n % 8192) as u64,
                }),
            );
        if indexes {
            files.update(&results);
            directory.update(&results, &changes);
            directory.sync(&results);
            black_box(directory.len());
        }
    }
    let elapsed = started.elapsed();
    assert_eq!(results.files, FILES);
    let expected: u64 = (0..FILES).map(|n| (n % 8192) as u64).sum();
    assert_eq!(results.bytes(0), expected);
    assert_eq!(results.bytes(depth), expected);
    if indexes {
        directory.sync(&results);
        assert_eq!(directory.len(), FILES);
        assert!((0..directory.len() - 1).all(|index| {
            let a = directory.id(index);
            let b = directory.id(index + 1);
            results.bytes(a) > results.bytes(b)
                || (results.bytes(a) == results.bytes(b)
                    && results.name_bytes(a) < results.name_bytes(b))
        }));
    }
    println!("{FILES} files, depth={depth}, indexes={indexes}: {elapsed:?}");
}

struct TrackingAllocator;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);

fn allocated(bytes: usize) {
    let live = LIVE_BYTES.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for TrackingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let pointer = unsafe { System.realloc(pointer, layout, size) };
        if !pointer.is_null() {
            LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
            allocated(size);
        }
        pointer
    }
}

#[global_allocator]
static ALLOCATOR: TrackingAllocator = TrackingAllocator;

fn report_memory(stage: &str, baseline: usize) {
    let live = LIVE_BYTES.load(Ordering::Relaxed) - baseline;
    let peak = PEAK_BYTES.load(Ordering::Relaxed) - baseline;
    println!(
        "{stage}: live={:.2} MiB, peak={:.2} MiB",
        live as f64 / 1048576.0,
        peak as f64 / 1048576.0
    );
}

fn synthetic_size(id: usize) -> u64 {
    // A long tail: most entries are small, roughly one in a thousand is large.
    if id % 997 == 0 {
        (((id % 4096) as u64) + 1) * 1024 * 1024
    } else {
        (((id % 97) as u64) + 1) * 512
    }
}

#[derive(Clone, Copy)]
enum NamePattern {
    /// Every entry has a distinct name: the dictionary's worst case.
    Unique,
    /// A mix with roughly the measured home-directory distinct ratio (~37%)
    /// and average raw name length.
    Repeated,
}

fn entry_name(pattern: NamePattern, id: usize) -> String {
    match pattern {
        NamePattern::Unique => format!("file-{id:09}"),
        NamePattern::Repeated if id % 3 == 0 => format!("unique-{id:010}"),
        NamePattern::Repeated => format!("com.apple.shared-cache-{:04}", id % 250_000),
    }
}

fn measure_memory(count: usize, pattern: NamePattern) {
    let label = match pattern {
        NamePattern::Unique => "all names unique (dictionary worst case)",
        NamePattern::Repeated => "repeated names (~40% distinct)",
    };
    println!("{count} files, {label}; requested heap only, not RSS");
    let prefix = PathBuf::from(format!("/root/{}", "d".repeat(100)));
    let baseline = LIVE_BYTES.load(Ordering::Relaxed);
    PEAK_BYTES.store(baseline, Ordering::Relaxed);
    let mut results = ScanResults::scanning();
    results.apply_batch([ScanEvent::Entry {
        path: prefix.clone(),
        parent: None,
        kind: EntryKind::Directory,
        bytes: 0,
        hard_link_to: None,
    }]);
    let mut directory = DirectoryIndex::default();
    directory.select(&results, 0);
    let mut files = LargeFileIndex::default();
    // Only multiply linked files need a deduplication table in the real
    // scanner; model a small fraction of them here.
    let mut identities = HashMap::new();
    for first in (0..count).step_by(128) {
        let changes = results.apply_batch((first..(first + 128).min(count)).map(|id| {
            if id % 10_000 == 0 {
                identities.insert((1_u64, id as u64), id + 1);
            }
            ScanEvent::Entry {
                path: prefix.join(entry_name(pattern, id)),
                parent: Some(0),
                kind: EntryKind::File,
                bytes: synthetic_size(id),
                hard_link_to: None,
            }
        }));
        directory.update(&results, &changes);
        files.update(&results);
        if id_visible_refresh(first) {
            directory.sync(&results);
        }
    }
    directory.sync(&results);
    report_memory("scanning with dedup + two indexes", baseline);
    drop(identities);
    let changes = results.apply_batch([
        ScanEvent::DirectoryRead(0),
        ScanEvent::Finished(ScanState::Complete),
    ]);
    directory.update(&results, &changes);
    files.update(&results);
    directory.sync(&results);
    report_memory("finished with two indexes", baseline);
    assert_eq!(directory.len(), count);
    let expected: u64 = (0..count).map(synthetic_size).sum();
    assert_eq!(results.bytes(0), expected);
    files.sync(&results, analysis::LARGE_FILE_FLOOR);
    black_box((
        &results,
        &directory,
        &files,
        files.visible_len(&results, analysis::LARGE_FILE_FLOOR),
    ));
}

/// The UI merges newly discovered children a few times per second, not once
/// per batch. Refreshing less often keeps the pending buffer small while
/// avoiding an O(n) merge for every 128 entries.
fn id_visible_refresh(first: usize) -> bool {
    first % (128 * 1024) == 0
}

fn main() {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("--memory") {
        let count = args
            .next()
            .map_or(1_000_000, |count| count.parse().expect("file count"));
        measure_memory(count, NamePattern::Unique);
        measure_memory(count, NamePattern::Repeated);
    } else {
        for _ in 0..3 {
            measure(0, true);
            measure(96, false);
        }
    }
}
