//! Synthetic in-memory performance check; does not measure filesystem I/O.
//! Run: rustc --edition=2024 -O scripts/core_perf.rs -o /tmp/clean-core-perf
//!      /tmp/clean-core-perf
#![allow(dead_code)]

#[path = "../src/analysis/mod.rs"]
mod analysis;
#[path = "../src/models/mod.rs"]
mod models;

use std::{hint::black_box, time::Instant};

use analysis::{DirectoryIndex, LargeFileIndex};
use models::{EntryKind, ScanEvent, ScanResults};

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
            black_box(directory.sorted());
        }
    }
    let elapsed = started.elapsed();
    assert_eq!(results.files, FILES);
    let expected: u64 = (0..FILES).map(|n| (n % 8192) as u64).sum();
    assert_eq!(results.nodes[0].bytes, expected);
    assert_eq!(results.nodes[depth].bytes, expected);
    if indexes {
        let sorted = directory.sorted();
        assert_eq!(sorted.len(), FILES);
        assert!(sorted.windows(2).all(|pair| {
            let a = &results.nodes[pair[0]];
            let b = &results.nodes[pair[1]];
            a.bytes > b.bytes || (a.bytes == b.bytes && a.path < b.path)
        }));
    }
    println!("{FILES} files, depth={depth}, indexes={indexes}: {elapsed:?}");
}

fn main() {
    for _ in 0..3 {
        measure(0, true);
        measure(96, false);
    }
}
