use super::*;
use crate::models::{EntryKind, ScanEvent, ScanState};

fn entry(results: &mut ScanResults, path: &str, kind: EntryKind, bytes: u64) {
    results.apply(ScanEvent::Entry {
        hard_link_to: None,
        path: path.into(),
        parent: (!results.is_empty()).then_some(0),
        kind,
        bytes,
    });
}

fn child(results: &mut ScanResults, parent: NodeId, path: &str, kind: EntryKind, bytes: u64) {
    results.apply(ScanEvent::Entry {
        hard_link_to: None,
        path: path.into(),
        parent: Some(parent),
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
    assert_eq!(index.matching(&results, 100), vec![3, 2, 1]);
    assert_eq!(index.matching(&results, 0), vec![3, 2, 1, 4]);
    assert!(index.matching(&results, 301).is_empty());
}

#[test]
fn large_file_index_keeps_only_files_at_or_above_the_floor() {
    let mut results = ScanResults::default();
    entry(&mut results, "/root", EntryKind::Directory, 0);
    entry(
        &mut results,
        "/root/large",
        EntryKind::File,
        LARGE_FILE_FLOOR + 512,
    );
    entry(&mut results, "/root/small", EntryKind::File, 1024);
    let mut index = LargeFileIndex::default();
    index.update(&results);
    assert_eq!(index.entries, vec![1]);
    assert_eq!(index.matching(&results, LARGE_FILE_FLOOR), vec![1]);
    assert_eq!(index.matching(&results, 0), vec![1, 2]);
}

#[test]
fn index_adds_nested_files_without_duplicates_and_keeps_partial_results() {
    let mut results = ScanResults::default();
    let mut index = LargeFileIndex::default();
    index.update(&results);
    assert!(index.matching(&results, 0).is_empty());
    entry(&mut results, "/root", EntryKind::Directory, 0);
    child(&mut results, 0, "/root/nested", EntryKind::Directory, 0);
    child(&mut results, 1, "/root/nested/file", EntryKind::File, 200);
    index.update(&results);
    index.update(&results);
    assert_eq!(index.matching(&results, 100), vec![2]);
    child(&mut results, 1, "/root/nested/new", EntryKind::File, 400);
    results.apply(ScanEvent::Finished(ScanState::Partial));
    index.update(&results);
    assert_eq!(index.matching(&results, 100), vec![3, 2]);
    assert_eq!(index.matching(&results, 300), vec![3]);
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
    child(&mut results, 0, "/root/z", EntryKind::Directory, 0);
    child(&mut results, 0, "/root/a", EntryKind::Directory, 0);
    results.apply_batch([ScanEvent::DirectoryRead(0)]);
    let mut index = DirectoryIndex::default();
    index.select(&results, 0);
    // Both directories start at zero bytes, so names decide the order.
    assert_eq!(index.sorted(&results), vec![2, 1]);
    let changes = results.apply_batch([
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root/z/large".into(),
            parent: Some(1),
            kind: EntryKind::File,
            bytes: 300,
        },
        ScanEvent::DirectoryRead(1),
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root/a/small".into(),
            parent: Some(2),
            kind: EntryKind::File,
            bytes: 20,
        },
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root/a/new".into(),
            parent: Some(2),
            kind: EntryKind::File,
            bytes: 200,
        },
        ScanEvent::DirectoryRead(2),
    ]);
    index.update(&results, &changes);
    assert_eq!(index.sorted(&results), vec![1, 2]);
    assert_eq!(index.sorted(&results), sorted_children(&results, 0));
    index.select(&results, 2);
    assert_eq!(index.sorted(&results), vec![5, 4]);
    index.select(&results, 1);
    assert_eq!(index.sorted(&results), vec![3]);
    index.select(&results, 0);
    assert_eq!(index.sorted(&results), sorted_children(&results, 0));
}

#[test]
fn directory_index_stores_four_byte_ids() {
    let mut results = ScanResults::default();
    entry(&mut results, "/root", EntryKind::Directory, 0);
    for id in 0..64 {
        entry(
            &mut results,
            &format!("/root/file-{id}"),
            EntryKind::File,
            id,
        );
    }
    let mut index = DirectoryIndex::default();
    index.select(&results, 0);
    assert_eq!(index.entries.len(), 64);
    assert_eq!(std::mem::size_of_val(&index.entries[0]), 4);
}

#[test]
fn large_file_index_excludes_hard_link_aliases_but_keeps_zero_allocation_files() {
    let mut results = ScanResults::default();
    entry(&mut results, "/root", EntryKind::Directory, 512);
    entry(&mut results, "/root/original", EntryKind::File, 4096);
    entry(&mut results, "/root/sparse", EntryKind::File, 0);
    let mut index = LargeFileIndex::default();
    index.update(&results);
    results.apply(ScanEvent::Entry {
        path: "/root/alias".into(),
        parent: Some(0),
        kind: EntryKind::File,
        bytes: 0,
        hard_link_to: Some(1),
    });
    index.update(&results);
    index.update(&results);
    assert_eq!(index.matching(&results, 0), vec![1, 2]);
    assert_eq!(index.matching(&results, 1), vec![1]);
    assert_eq!(results.files, 3);
    assert_eq!(results.hard_links, 1);
    assert_eq!(results.bytes(0), 512 + 4096);
    results.apply(ScanEvent::Finished(ScanState::Partial));
    index.update(&results);
    assert_eq!(index.matching(&results, 0), vec![1, 2]);
    assert_eq!(index.matching(&results, 1), vec![1]);
}

#[test]
fn terminal_states_keep_compact_index_results_stable() {
    for state in [ScanState::Complete, ScanState::Partial, ScanState::Failed] {
        let mut results = ScanResults::scanning();
        entry(&mut results, "/root", EntryKind::Directory, 0);
        entry(&mut results, "/root/z", EntryKind::File, 100);
        entry(&mut results, "/root/a", EntryKind::File, 100);
        let mut files = LargeFileIndex::default();
        files.update(&results);
        let mut directory = DirectoryIndex::default();
        directory.select(&results, 0);
        let changes = results.apply_batch([
            ScanEvent::Entry {
                path: "/root/largest".into(),
                parent: Some(0),
                kind: EntryKind::File,
                bytes: 200,
                hard_link_to: None,
            },
            ScanEvent::Finished(state),
        ]);
        files.update(&results);
        directory.update(&results, &changes);
        assert_eq!(files.matching(&results, 100), vec![3, 2, 1]);
        assert_eq!(files.matching(&results, 101), vec![3]);
        assert!(files.matching(&results, 201).is_empty());
        assert_eq!(directory.sorted(&results), vec![3, 2, 1]);
        files.update(&results);
        directory.update(&results, &[]);
        assert_eq!(directory.sorted(&results), vec![3, 2, 1]);
    }
}

#[test]
fn finished_directory_navigation_builds_only_id_vectors() {
    let mut results = ScanResults::default();
    entry(&mut results, "/root", EntryKind::Directory, 0);
    child(&mut results, 0, "/root/z", EntryKind::Directory, 0);
    child(&mut results, 0, "/root/a", EntryKind::Directory, 0);
    results.apply_batch([
        ScanEvent::Entry {
            path: "/root/z/file".into(),
            parent: Some(1),
            kind: EntryKind::File,
            bytes: 100,
            hard_link_to: None,
        },
        ScanEvent::Entry {
            path: "/root/a/file".into(),
            parent: Some(2),
            kind: EntryKind::File,
            bytes: 100,
            hard_link_to: None,
        },
        ScanEvent::Finished(ScanState::Complete),
    ]);
    let mut directory = DirectoryIndex::default();
    for (id, expected) in [(0, vec![2, 1]), (1, vec![3]), (2, vec![4]), (0, vec![2, 1])] {
        directory.select(&results, id);
        assert_eq!(directory.sorted(&results), expected);
        assert_eq!(std::mem::size_of_val(&directory.entries[0]), 4);
    }
    let mut files = LargeFileIndex::default();
    files.update(&results);
    assert_eq!(files.matching(&results, 100), vec![4, 3]);
}

#[test]
fn index_outputs_reuse_existing_buffer_before_and_after_completion() {
    let mut results = ScanResults::default();
    entry(&mut results, "/root", EntryKind::Directory, 0);
    entry(&mut results, "/root/z", EntryKind::File, 100);
    entry(&mut results, "/root/a", EntryKind::File, 0);
    let mut files = LargeFileIndex::default();
    files.update(&results);
    let mut directory = DirectoryIndex::default();
    directory.select(&results, 0);
    let mut output: Vec<u32> = Vec::with_capacity(16);
    let pointer = output.as_ptr();
    let capacity = output.capacity();
    for state in [ScanState::Idle, ScanState::Complete] {
        results.state = state;
        files.update(&results);
        directory.update(&results, &[]);
        directory.write_sorted(&results, &mut output);
        assert_eq!(output, vec![1, 2]);
        assert_eq!(output.as_ptr(), pointer);
        assert_eq!(output.capacity(), capacity);
        files.write_matching(&results, 100, &mut output);
        assert_eq!(output, vec![1]);
        assert_eq!(output.as_ptr(), pointer);
        assert_eq!(output.capacity(), capacity);
        files.write_matching(&results, 101, &mut output);
        assert!(output.is_empty());
        files.write_matching(&results, 0, &mut output);
        assert_eq!(output, vec![1, 2]);
    }
}
