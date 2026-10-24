use super::*;
use crate::models::{EntryKind, ScanEvent, ScanState};

fn entry(results: &mut ScanResults, path: &str, kind: EntryKind, bytes: u64) {
    results.apply(ScanEvent::Entry {
        path: path.into(),
        parent: (!results.nodes.is_empty()).then_some(0),
        kind,
        bytes,
    });
}

#[test]
fn large_files_include_boundary_and_sort_ties_by_path() {
    let mut results = ScanResults::default();
    entry(&mut results, "/root", EntryKind::Directory, 0);
    entry(&mut results, "/root/z", EntryKind::File, 100);
    entry(&mut results, "/root/a", EntryKind::File, 100);
    entry(&mut results, "/root/largest", EntryKind::File, 300);
    entry(&mut results, "/root/small", EntryKind::File, 99);
    entry(&mut results, "/root/link", EntryKind::Symlink, 0);
    entry(&mut results, "/root/special", EntryKind::Other, 0);
    let mut index = LargeFileIndex::default();
    index.update(&results);
    assert_eq!(index.matching(100), vec![3, 2, 1]);
    assert_eq!(index.matching(0), vec![3, 2, 1, 4]);
    assert!(index.matching(301).is_empty());
}

#[test]
fn index_adds_nested_files_without_duplicates_and_keeps_partial_results() {
    let mut results = ScanResults::default();
    let mut index = LargeFileIndex::default();
    index.update(&results);
    assert!(index.matching(0).is_empty());
    entry(&mut results, "/root", EntryKind::Directory, 0);
    entry(&mut results, "/root/nested", EntryKind::Directory, 0);
    results.apply(ScanEvent::Entry {
        path: "/root/nested/file".into(),
        parent: Some(1),
        kind: EntryKind::File,
        bytes: 200,
    });
    index.update(&results);
    index.update(&results);
    assert_eq!(index.matching(100), vec![2]);
    entry(&mut results, "/root/new", EntryKind::File, 400);
    results.apply(ScanEvent::Finished(ScanState::Partial));
    index.update(&results);
    assert_eq!(index.matching(100), vec![3, 2]);
    assert_eq!(index.matching(300), vec![3]);
}

#[test]
fn byte_formatting_uses_binary_units() {
    assert_eq!(format_bytes(0), "0 B");
    assert_eq!(format_bytes(1023), "1023 B");
    assert_eq!(format_bytes(1024), "1.0 KiB");
    assert_eq!(format_bytes(1536), "1.5 KiB");
    assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
}

#[test]
fn directory_index_tracks_aggregate_size_changes_and_navigation() {
    let mut results = ScanResults::default();
    entry(&mut results, "/root", EntryKind::Directory, 0);
    entry(&mut results, "/root/z", EntryKind::Directory, 0);
    entry(&mut results, "/root/a", EntryKind::Directory, 0);
    let mut index = DirectoryIndex::default();
    index.select(&results, 0);
    assert_eq!(index.sorted(), sorted_children(&results, 0));
    let changes = results.apply_batch([
        ScanEvent::Entry {
            path: "/root/z/large".into(),
            parent: Some(1),
            kind: EntryKind::File,
            bytes: 100,
        },
        ScanEvent::Entry {
            path: "/root/a/small".into(),
            parent: Some(2),
            kind: EntryKind::File,
            bytes: 20,
        },
        ScanEvent::Entry {
            path: "/root/top".into(),
            parent: Some(0),
            kind: EntryKind::File,
            bytes: 50,
        },
    ]);
    index.update(&results, &changes);
    assert_eq!(index.sorted(), vec![1, 5, 2]);
    assert_eq!(index.sorted(), sorted_children(&results, 0));
    let changes = results.apply_batch([
        ScanEvent::Entry {
            path: "/root/a/new".into(),
            parent: Some(2),
            kind: EntryKind::File,
            bytes: 200,
        },
        ScanEvent::DirectoryRead(2),
    ]);
    index.update(&results, &changes);
    assert_eq!(index.sorted(), vec![2, 1, 5]);
    index.select(&results, 2);
    assert_eq!(index.sorted(), vec![6, 4]);
    index.select(&results, 0);
    assert_eq!(index.sorted(), sorted_children(&results, 0));
}

#[test]
fn size_indexes_share_node_paths_instead_of_copying_their_contents() {
    let mut results = ScanResults::default();
    entry(&mut results, "/root", EntryKind::Directory, 0);
    entry(&mut results, "/root/file", EntryKind::File, 100);
    let mut files = LargeFileIndex::default();
    files.update(&results);
    let mut directory = DirectoryIndex::default();
    directory.select(&results, 0);
    assert!(Arc::ptr_eq(
        &results.nodes[1].path,
        &files.files.first().unwrap().1
    ));
    assert!(Arc::ptr_eq(
        &results.nodes[1].path,
        &directory.entries.first().unwrap().1
    ));
}
