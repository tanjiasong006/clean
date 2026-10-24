//! Synthetic in-memory performance check; does not measure filesystem I/O.
//! Run: rustc --edition=2024 -O scripts/core_perf.rs -o /tmp/clean-core-perf
//!      /tmp/clean-core-perf
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
use models::{EntryKind, Node, ScanEvent, ScanResults, ScanState};

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
    let mut visible = Vec::new();
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
            directory.write_sorted(&mut visible);
            black_box(&visible);
        }
    }
    let elapsed = started.elapsed();
    assert_eq!(results.files, FILES);
    let expected: u64 = (0..FILES).map(|n| (n % 8192) as u64).sum();
    assert_eq!(results.nodes[0].bytes, expected);
    assert_eq!(results.nodes[depth].bytes, expected);
    if indexes {
        directory.write_sorted(&mut visible);
        let sorted = &visible;
        assert_eq!(sorted.len(), FILES);
        assert!(sorted.windows(2).all(|pair| {
            let a = &results.nodes[pair[0]];
            let b = &results.nodes[pair[1]];
            a.bytes > b.bytes || (a.bytes == b.bytes && a.path < b.path)
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

fn measure_memory(count: usize) {
    println!(
        "{count} files; Node={} bytes; requested heap only, not RSS",
        std::mem::size_of::<Node>()
    );
    let prefix = PathBuf::from(format!("/root/{}", "d".repeat(100)));
    let baseline = LIVE_BYTES.load(Ordering::Relaxed);
    PEAK_BYTES.store(baseline, Ordering::Relaxed);
    let mut results = ScanResults {
        state: ScanState::Scanning,
        ..Default::default()
    };
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
    let mut identities = HashMap::new();
    let mut visible = Vec::new();
    for first in (0..count).step_by(128) {
        let changes = results.apply_batch((first..(first + 128).min(count)).map(|id| {
            identities.insert((1_u64, id as u64), id + 1);
            ScanEvent::Entry {
                path: prefix.join(format!("file-{id:09}")),
                parent: Some(0),
                kind: EntryKind::File,
                bytes: (id as u64 + 1) * 4096,
                hard_link_to: None,
            }
        }));
        directory.update(&results, &changes);
        files.update(&results);
    }
    directory.write_sorted(&mut visible);
    report_memory("scanning with dedup + two indexes + visible IDs", baseline);
    drop(identities);
    let changes = results.apply_batch([
        ScanEvent::DirectoryRead(0),
        ScanEvent::Finished(ScanState::Complete),
    ]);
    directory.update(&results, &changes);
    files.update(&results);
    directory.write_sorted(&mut visible);
    report_memory("finished with two indexes + visible IDs", baseline);
    assert_eq!(visible.len(), count);
    assert_eq!(visible.first().copied(), (count > 0).then_some(count));
    files.write_matching(&results, 0, &mut visible);
    assert_eq!(visible.len(), count);
    assert_eq!(
        results.nodes[0].bytes,
        (1..=count as u64).sum::<u64>() * 4096
    );
    black_box((&results, &directory, &files, &visible));
}

fn main() {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() == Some("--memory") {
        measure_memory(
            args.next()
                .map_or(1_000_000, |count| count.parse().expect("file count")),
        );
    } else {
        for _ in 0..3 {
            measure(0, true);
            measure(96, false);
        }
    }
}
