use super::*;
use crate::models::{EntryKind, ScanEvent, ScanState};

fn entry(results: &mut ScanResults, path: &str, kind: EntryKind, bytes: u64) {
    results.apply(ScanEvent::Entry {
        hard_link_to: None,
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
    assert_eq!(index.matching(&results, 100), vec![3, 2, 1]);
    assert_eq!(index.matching(&results, 0), vec![3, 2, 1, 4]);
    assert!(index.matching(&results, 301).is_empty());
}

#[test]
fn index_adds_nested_files_without_duplicates_and_keeps_partial_results() {
    let mut results = ScanResults::default();
    let mut index = LargeFileIndex::default();
    index.update(&results);
    assert!(index.matching(&results, 0).is_empty());
    entry(&mut results, "/root", EntryKind::Directory, 0);
    entry(&mut results, "/root/nested", EntryKind::Directory, 0);
    results.apply(ScanEvent::Entry {
        hard_link_to: None,
        path: "/root/nested/file".into(),
        parent: Some(1),
        kind: EntryKind::File,
        bytes: 200,
    });
    index.update(&results);
    index.update(&results);
    assert_eq!(index.matching(&results, 100), vec![2]);
    entry(&mut results, "/root/new", EntryKind::File, 400);
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
    entry(&mut results, "/root/z", EntryKind::Directory, 0);
    entry(&mut results, "/root/a", EntryKind::Directory, 0);
    let mut index = DirectoryIndex::default();
    index.select(&results, 0);
    assert_eq!(index.sorted(), sorted_children(&results, 0));
    let changes = results.apply_batch([
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root/z/large".into(),
            parent: Some(1),
            kind: EntryKind::File,
            bytes: 100,
        },
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root/a/small".into(),
            parent: Some(2),
            kind: EntryKind::File,
            bytes: 20,
        },
        ScanEvent::Entry {
            hard_link_to: None,
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
            hard_link_to: None,
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
    let SizeIndex::Live(files) = &files.files else {
        panic!("live file index")
    };
    let SizeIndex::Live(entries) = &directory.entries else {
        panic!("live directory index")
    };
    assert!(Arc::ptr_eq(
        &results.nodes[1].path,
        &files.first().unwrap().1
    ));
    assert!(Arc::ptr_eq(
        &results.nodes[1].path,
        &entries.first().unwrap().1
    ));
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
    assert_eq!(results.nodes[0].bytes, 512 + 4096);
    results.apply(ScanEvent::Finished(ScanState::Partial));
    index.update(&results);
    assert!(matches!(index.files, SizeIndex::Finished(_)));
    assert_eq!(index.matching(&results, 0), vec![1, 2]);
    assert_eq!(index.matching(&results, 1), vec![1]);
}

#[test]
fn terminal_states_compact_both_indexes_and_release_path_references() {
    for state in [ScanState::Complete, ScanState::Partial, ScanState::Failed] {
        let mut results = ScanResults {
            state: ScanState::Scanning,
            ..Default::default()
        };
        entry(&mut results, "/root", EntryKind::Directory, 0);
        entry(&mut results, "/root/z", EntryKind::File, 100);
        entry(&mut results, "/root/a", EntryKind::File, 100);
        let mut files = LargeFileIndex::default();
        files.update(&results);
        let mut directory = DirectoryIndex::default();
        directory.select(&results, 0);
        assert_eq!(Arc::strong_count(&results.nodes[1].path), 3);
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
        let SizeIndex::Finished(ids) = &files.files else {
            panic!("compact file index")
        };
        assert_eq!(ids, &[3, 2, 1]);
        assert_eq!(ids.capacity(), ids.len());
        let SizeIndex::Finished(ids) = &directory.entries else {
            panic!("compact directory index")
        };
        assert_eq!(ids, &[3, 2, 1]);
        assert_eq!(ids.capacity(), ids.len());
        assert_eq!(Arc::strong_count(&results.nodes[1].path), 1);
        assert_eq!(files.matching(&results, 100), vec![3, 2, 1]);
        assert_eq!(files.matching(&results, 101), vec![3]);
        assert!(files.matching(&results, 201).is_empty());
        files.update(&results);
        directory.update(&results, &[]);
        assert_eq!(directory.sorted(), vec![3, 2, 1]);
    }
}

#[test]
fn finished_directory_navigation_builds_only_id_vectors() {
    let mut results = ScanResults::default();
    entry(&mut results, "/root", EntryKind::Directory, 0);
    entry(&mut results, "/root/z", EntryKind::Directory, 0);
    entry(&mut results, "/root/a", EntryKind::Directory, 0);
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
        assert!(matches!(directory.entries, SizeIndex::Finished(_)));
        assert_eq!(directory.sorted(), expected);
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
    let mut output = Vec::with_capacity(16);
    let pointer = output.as_ptr();
    let capacity = output.capacity();
    for state in [ScanState::Idle, ScanState::Complete] {
        results.state = state;
        files.update(&results);
        directory.update(&results, &[]);
        directory.write_sorted(&mut output);
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
