use super::*;

#[derive(Default)]
struct Reference {
    nodes: Vec<(Option<NodeId>, u64, usize, usize)>,
    files: usize,
    directories: usize,
    symlinks: usize,
    issues: usize,
    state: ScanState,
}

impl Reference {
    // Deliberately use the simple per-event ancestor walk as an independent
    // oracle for aggregation across arbitrary batch boundaries.
    fn apply(&mut self, event: &ScanEvent) {
        match event {
            ScanEvent::Entry {
                parent,
                kind,
                bytes,
                ..
            } => {
                let pending = usize::from(*kind == EntryKind::Directory);
                self.nodes.push((*parent, *bytes, pending, 0));
                match kind {
                    EntryKind::File => self.files += 1,
                    EntryKind::Directory => self.directories += 1,
                    EntryKind::Symlink => self.symlinks += 1,
                    EntryKind::Other => {}
                }
                let mut ancestor = *parent;
                while let Some(id) = ancestor {
                    self.nodes[id].1 = self.nodes[id].1.saturating_add(*bytes);
                    self.nodes[id].2 += pending;
                    ancestor = self.nodes[id].0;
                }
            }
            ScanEvent::DirectoryRead(id) => {
                let mut ancestor = Some(*id);
                while let Some(id) = ancestor {
                    self.nodes[id].2 -= 1;
                    ancestor = self.nodes[id].0;
                }
            }
            ScanEvent::Issue { parent, .. } => {
                self.issues += 1;
                let mut ancestor = Some(*parent).filter(|id| *id < self.nodes.len());
                while let Some(id) = ancestor {
                    self.nodes[id].3 += 1;
                    ancestor = self.nodes[id].0;
                }
            }
            ScanEvent::Finished(state) => self.state = *state,
        }
    }

    fn assert_matches(&self, results: &ScanResults) {
        assert_eq!(self.nodes.len(), results.len());
        for id in 0..results.len() {
            assert_eq!(
                self.nodes[id],
                (
                    results.parent(id),
                    results.bytes(id),
                    results.pending_directories(id),
                    results.errors(id)
                ),
                "node {id}"
            );
        }
        assert_eq!(
            (
                self.files,
                self.directories,
                self.symlinks,
                self.issues,
                self.state
            ),
            (
                results.files,
                results.directories,
                results.symlinks,
                results.issues.len(),
                results.state
            )
        );
    }
}

/// Emits each directory's entries as one contiguous run, matching what a
/// breadth-first traversal worker produces.
fn sample_events() -> Vec<ScanEvent> {
    let mut events = vec![ScanEvent::Entry {
        hard_link_to: None,
        path: "/root".into(),
        parent: None,
        kind: EntryKind::Directory,
        bytes: 0,
    }];
    let mut frontier = vec![0_usize];
    let mut next_id = 1_usize;
    let mut seed = 42_u64;
    while let Some(parent) = frontier.pop() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let count = 1 + (seed >> 33) as usize % 4;
        let mut children = Vec::new();
        for _ in 0..count {
            let id = next_id;
            next_id += 1;
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let kind = match id % 11 {
                0 => EntryKind::Directory,
                1 => EntryKind::Symlink,
                2 => EntryKind::Other,
                _ => EntryKind::File,
            };
            events.push(ScanEvent::Entry {
                hard_link_to: None,
                path: format!("/entry-{id}").into(),
                parent: Some(parent),
                kind,
                bytes: if kind == EntryKind::File {
                    (seed % 16) * 512
                } else {
                    0
                },
            });
            if kind == EntryKind::Directory {
                children.push(id);
            }
            if id.is_multiple_of(37) {
                events.push(ScanEvent::Issue {
                    parent,
                    issue: ScanIssue {
                        path: format!("/missing-{id}").into(),
                        message: "removed".into(),
                    },
                });
            }
        }
        events.push(ScanEvent::DirectoryRead(parent));
        frontier.extend(children);
    }
    events.push(ScanEvent::Issue {
        parent: 0,
        issue: ScanIssue {
            path: "/root/denied".into(),
            message: "denied".into(),
        },
    });
    events.push(ScanEvent::Finished(ScanState::Partial));
    events
}

#[test]
fn batched_aggregation_matches_per_event_oracle_at_every_boundary() {
    for batch_size in [1, 2, 3, 17, 128, 1024, 4096] {
        let mut results = ScanResults::default();
        let mut reference = Reference::default();
        let mut pending = Vec::new();
        for event in sample_events() {
            reference.apply(&event);
            pending.push(event);
            if pending.len() == batch_size {
                results.apply_batch(pending.drain(..));
                reference.assert_matches(&results);
            }
        }
        results.apply_batch(pending);
        reference.assert_matches(&results);
        assert!(
            (0..results.len()).all(|id| results.pending_directories(id) == 0),
            "every directory is closed"
        );
        assert!(results.errors(0) > 0);
    }
}

#[test]
fn changes_contain_each_affected_node_once_with_its_previous_size() {
    let mut results = ScanResults::default();
    results.apply_batch([
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root".into(),
            parent: None,
            kind: EntryKind::Directory,
            bytes: 0,
        },
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root/nested".into(),
            parent: Some(0),
            kind: EntryKind::Directory,
            bytes: 0,
        },
    ]);
    let changes = results.apply_batch((0..1024).map(|id| ScanEvent::Entry {
        hard_link_to: None,
        path: format!("/root/nested/{id}").into(),
        parent: Some(1),
        kind: EntryKind::File,
        bytes: 5120,
    }));
    assert_eq!(changes.len(), 1026);
    let unique: std::collections::BTreeSet<_> = changes.iter().map(|change| change.id).collect();
    assert_eq!(unique.len(), changes.len());
    assert_eq!(
        changes
            .iter()
            .filter(|change| change.previous_bytes == Some(0))
            .count(),
        2
    );
    assert_eq!(
        changes
            .iter()
            .filter(|change| change.previous_bytes.is_none())
            .count(),
        1024
    );
    assert_eq!(results.bytes(0), 1024 * 5120);
    assert_eq!(results.bytes(1), 1024 * 5120);
}

#[test]
fn unfinished_subtrees_and_read_failures_stay_incomplete_in_the_same_batch() {
    let mut results = ScanResults::default();
    results.apply_batch([
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root".into(),
            parent: None,
            kind: EntryKind::Directory,
            bytes: 0,
        },
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root/unread".into(),
            parent: Some(0),
            kind: EntryKind::Directory,
            bytes: 0,
        },
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root/denied".into(),
            parent: Some(0),
            kind: EntryKind::Directory,
            bytes: 0,
        },
        ScanEvent::DirectoryRead(0),
        ScanEvent::Issue {
            parent: 2,
            issue: ScanIssue {
                path: "/root/denied".into(),
                message: "denied".into(),
            },
        },
        ScanEvent::DirectoryRead(2),
        ScanEvent::Finished(ScanState::Partial),
    ]);
    assert_eq!(results.pending_directories(0), 1);
    assert_eq!(results.errors(0), 1);
    assert!(!results.is_complete(0));
    assert!(!results.is_complete(1));
    assert!(!results.is_complete(2));
}

#[test]
fn size_overflow_saturates_without_wrapping_or_panicking() {
    let mut results = ScanResults::default();
    results.apply_batch([
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root".into(),
            parent: None,
            kind: EntryKind::Directory,
            bytes: 0,
        },
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root/large".into(),
            parent: Some(0),
            kind: EntryKind::File,
            bytes: u64::MAX,
        },
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root/one".into(),
            parent: Some(0),
            kind: EntryKind::File,
            bytes: 1,
        },
        ScanEvent::DirectoryRead(0),
    ]);
    assert_eq!(results.bytes(0), u64::MAX);
    assert_eq!(results.files, 2);
}

#[test]
fn overflow_is_reported_once_and_cannot_be_finished_as_an_exact_complete_scan() {
    for batch_size in [1, 2, 128] {
        let mut results = ScanResults::default();
        let mut events = vec![
            ScanEvent::Entry {
                hard_link_to: None,
                path: "/root".into(),
                parent: None,
                kind: EntryKind::Directory,
                bytes: 0,
            },
            ScanEvent::Entry {
                hard_link_to: None,
                path: "/root/dir".into(),
                parent: Some(0),
                kind: EntryKind::Directory,
                bytes: 0,
            },
            ScanEvent::Entry {
                hard_link_to: None,
                path: "/root/dir/a".into(),
                parent: Some(1),
                kind: EntryKind::File,
                bytes: u64::MAX,
            },
            ScanEvent::Entry {
                hard_link_to: None,
                path: "/root/dir/b".into(),
                parent: Some(1),
                kind: EntryKind::File,
                bytes: 1,
            },
            ScanEvent::Entry {
                hard_link_to: None,
                path: "/root/dir/c".into(),
                parent: Some(1),
                kind: EntryKind::File,
                bytes: 1,
            },
            ScanEvent::DirectoryRead(0),
            ScanEvent::DirectoryRead(1),
            ScanEvent::Finished(ScanState::Complete),
        ]
        .into_iter();
        loop {
            let batch: Vec<_> = events.by_ref().take(batch_size).collect();
            if batch.is_empty() {
                break;
            }
            results.apply_batch(batch);
        }
        assert_eq!(results.state, ScanState::Partial);
        assert_eq!(results.issues.len(), 1);
        for id in 0..2 {
            assert!(results.size_overflow(id));
            assert!(!results.is_complete(id));
        }
        assert!(!results.size_overflow(2));
    }
}

#[test]
fn directory_allocation_and_hard_link_ownership_survive_batch_boundaries() {
    for batch_size in [1, 2, 3, 128] {
        let mut results = ScanResults::default();
        let mut events = vec![
            ScanEvent::Entry {
                path: "/root".into(),
                parent: None,
                kind: EntryKind::Directory,
                bytes: 512,
                hard_link_to: None,
            },
            ScanEvent::Entry {
                path: "/root/file".into(),
                parent: Some(0),
                kind: EntryKind::File,
                bytes: 4096,
                hard_link_to: None,
            },
            ScanEvent::Entry {
                path: "/root/nested".into(),
                parent: Some(0),
                kind: EntryKind::Directory,
                bytes: 1024,
                hard_link_to: None,
            },
            ScanEvent::Entry {
                path: "/root/nested/alias".into(),
                parent: Some(2),
                kind: EntryKind::File,
                bytes: 0,
                hard_link_to: Some(1),
            },
            ScanEvent::DirectoryRead(0),
            ScanEvent::DirectoryRead(2),
            ScanEvent::Finished(ScanState::Complete),
        ]
        .into_iter();
        loop {
            let batch: Vec<_> = events.by_ref().take(batch_size).collect();
            if batch.is_empty() {
                break;
            }
            results.apply_batch(batch);
        }
        assert_eq!(results.state, ScanState::Complete);
        assert_eq!(results.bytes(0), 512 + 4096 + 1024);
        assert_eq!(results.bytes(2), 1024);
        assert_eq!(results.bytes(3), 0);
        assert_eq!(results.hard_link_to(3), Some(1));
        assert_eq!(results.files, 2);
        assert_eq!(results.hard_links, 1);
        assert!((0..results.len()).all(|id| results.is_complete(id)));
        assert!(results.issues.is_empty());
    }
}

#[test]
fn hard_link_aliases_are_indexed_in_node_order_across_batches() {
    let mut results = ScanResults::default();
    let entry = |path: &str, id: usize, target: Option<usize>| ScanEvent::Entry {
        path: path.into(),
        parent: (id > 0).then_some(0),
        kind: if id == 0 {
            EntryKind::Directory
        } else {
            EntryKind::File
        },
        bytes: if target.is_some() { 0 } else { 512 },
        hard_link_to: target,
    };
    results.apply_batch([entry("/root", 0, None), entry("/root/original", 1, None)]);
    results.apply_batch([entry("/root/alias-1", 2, Some(1))]);
    results.apply_batch([
        entry("/root/alias-2", 3, Some(1)),
        entry("/root/alias-3", 4, Some(1)),
    ]);
    assert_eq!(results.hard_links, 3);
    assert_eq!(results.hard_link_to(1), None);
    for alias in 2..=4 {
        assert_eq!(results.hard_link_to(alias), Some(1), "alias {alias}");
    }
    results.apply_batch([ScanEvent::Finished(ScanState::Complete)]);
    assert_eq!(results.hard_link_to(3), Some(1));
}

#[test]
fn nodes_store_names_in_chunks_and_reconstruct_exact_paths() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let root = PathBuf::from("/root/long/shared/prefix/");
    let name = OsString::from_vec(b"file-\xff".to_vec());
    let expected = [
        root.clone(),
        root.join("nested"),
        root.join("nested").join(&name),
    ];
    let mut results = ScanResults::default();
    for (id, path) in expected.iter().enumerate() {
        results.apply(ScanEvent::Entry {
            path: path.clone(),
            parent: id.checked_sub(1),
            kind: if id == 2 {
                EntryKind::File
            } else {
                EntryKind::Directory
            },
            bytes: 512,
            hard_link_to: None,
        });
    }
    for (id, path) in expected.iter().enumerate() {
        assert_eq!(results.to_path_buf(id), *path);
    }
    assert_eq!(results.name(2).to_os_string(), name);
    assert!(results.child_ids(2).is_empty());
    assert_eq!(results.pending_directories(2), 0);
    assert_eq!(results.errors(2), 0);
    assert!(results.is_complete(2));
    // Root, nested directory, and the file: the file has no directory data.
    assert_eq!(results.child_count(0), 1);
    assert_eq!(std::mem::size_of::<Node>(), 6);
}

#[test]
fn path_order_matches_filesystem_order_across_ancestors_and_trees() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let names = [
        OsString::from("a"),
        OsString::from("a-"),
        OsString::from("z"),
        OsString::from("中文"),
        OsString::from_vec(b"non-utf8-\xff".to_vec()),
    ];
    let mut results = ScanResults::default();
    let root = PathBuf::from("/root/./shared/");
    results.apply(ScanEvent::Entry {
        path: root.clone(),
        parent: None,
        kind: EntryKind::Directory,
        bytes: 0,
        hard_link_to: None,
    });
    let mut expected = vec![root];
    let mut frontier = vec![0_usize];
    let mut seed = 73_u64;
    while let Some(parent) = frontier.pop() {
        if expected.len() >= 256 {
            break;
        }
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let count = (seed >> 40) as usize % 4;
        let mut children = Vec::new();
        for _ in 0..count {
            if expected.len() >= 256 {
                break;
            }
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let name = &names[(seed >> 32) as usize % names.len()];
            let path = expected[parent].join(name);
            let id = expected.len();
            results.apply(ScanEvent::Entry {
                path: path.clone(),
                parent: Some(parent),
                kind: EntryKind::Directory,
                bytes: 0,
                hard_link_to: None,
            });
            expected.push(path);
            children.push(id);
        }
        frontier.extend(children);
    }
    for (id, path) in expected.iter().enumerate() {
        assert_eq!(results.to_path_buf(id), *path);
    }
    for a in 0..expected.len() {
        for b in 0..expected.len() {
            assert_eq!(
                results.cmp_path(a, b),
                expected[a].cmp(&expected[b]),
                "{a}, {b}"
            );
        }
    }
}

#[test]
fn repeated_names_share_one_dictionary_entry() {
    let mut results = ScanResults::default();
    results.apply_batch([
        ScanEvent::Entry {
            path: "/root".into(),
            parent: None,
            kind: EntryKind::Directory,
            bytes: 0,
            hard_link_to: None,
        },
        ScanEvent::Entry {
            path: "/root/a".into(),
            parent: Some(0),
            kind: EntryKind::Directory,
            bytes: 0,
            hard_link_to: None,
        },
        ScanEvent::Entry {
            path: "/root/b".into(),
            parent: Some(0),
            kind: EntryKind::Directory,
            bytes: 0,
            hard_link_to: None,
        },
        ScanEvent::Entry {
            path: "/root/a/shared-name".into(),
            parent: Some(1),
            kind: EntryKind::File,
            bytes: 512,
            hard_link_to: None,
        },
        ScanEvent::Entry {
            path: "/root/b/shared-name".into(),
            parent: Some(2),
            kind: EntryKind::File,
            bytes: 1024,
            hard_link_to: None,
        },
        ScanEvent::Entry {
            path: "/root/b/other".into(),
            parent: Some(2),
            kind: EntryKind::File,
            bytes: 0,
            hard_link_to: None,
        },
    ]);
    // "/root", "a", "shared-name", "b", "other": the repeated name is stored once.
    assert_eq!(results.unique_names(), 5);
    assert_eq!(
        results.stored_name_bytes(),
        "/root".len() + "a".len() + "shared-name".len() + "b".len() + "other".len()
    );
    assert_eq!(results.name_id(3), results.name_id(4));
    assert_ne!(results.name_id(3), results.name_id(5));
    assert_eq!(results.name(4).to_string_lossy(), "shared-name");
    assert_eq!(results.name_bytes(4), &b"shared-name"[..]);
    assert_eq!(results.to_path_buf(4), PathBuf::from("/root/b/shared-name"));
}

#[test]
fn long_names_intern_exactly_across_the_length_byte_boundary() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let boundary = OsString::from_vec(vec![b'y'; 255]);
    let longer = OsString::from_vec((0..300).map(|index| b'a' + index as u8 % 26).collect());
    let mut results = ScanResults::default();
    results.apply_batch([
        ScanEvent::Entry {
            path: "/root".into(),
            parent: None,
            kind: EntryKind::Directory,
            bytes: 0,
            hard_link_to: None,
        },
        ScanEvent::Entry {
            path: std::path::Path::new("/root").join(&boundary),
            parent: Some(0),
            kind: EntryKind::File,
            bytes: 512,
            hard_link_to: None,
        },
        ScanEvent::Entry {
            path: std::path::Path::new("/root").join(&longer),
            parent: Some(0),
            kind: EntryKind::File,
            bytes: 512,
            hard_link_to: None,
        },
        ScanEvent::Entry {
            path: std::path::Path::new("/root").join(&longer),
            parent: Some(0),
            kind: EntryKind::File,
            bytes: 512,
            hard_link_to: None,
        },
    ]);
    assert_eq!(results.unique_names(), 3);
    assert_eq!(results.name(1).to_os_string(), boundary);
    assert_eq!(results.name(2).to_os_string(), longer);
    assert_eq!(results.name_id(2), results.name_id(3));
}

#[test]
fn directory_error_counts_are_not_truncated() {
    let mut results = ScanResults::default();
    results.apply(ScanEvent::Entry {
        path: "/root".into(),
        parent: None,
        kind: EntryKind::Directory,
        bytes: 0,
        hard_link_to: None,
    });
    for index in 0..300 {
        results.apply(ScanEvent::Issue {
            parent: 0,
            issue: ScanIssue {
                path: format!("/root/denied-{index}").into(),
                message: "permission denied".into(),
            },
        });
    }
    assert_eq!(results.errors(0), 300);
    assert!(!results.is_complete(0));
}

#[test]
fn files_above_the_inline_block_limit_keep_their_exact_size() {
    let sizes = [
        2 * 1024_u64 * 1024 * 1024 - 1024, // largest block count stored inline
        2 * 1024_u64 * 1024 * 1024,
        3 * 1024_u64 * 1024 * 1024 + 512,
    ];
    let mut results = ScanResults::default();
    let mut events = vec![ScanEvent::Entry {
        path: "/root".into(),
        parent: None,
        kind: EntryKind::Directory,
        bytes: 512,
        hard_link_to: None,
    }];
    for (index, bytes) in sizes.iter().enumerate() {
        events.push(ScanEvent::Entry {
            path: format!("/root/file-{index}").into(),
            parent: Some(0),
            kind: EntryKind::File,
            bytes: *bytes,
            hard_link_to: None,
        });
    }
    results.apply_batch(events);
    for (index, bytes) in sizes.iter().enumerate() {
        assert_eq!(results.bytes(index + 1), *bytes);
    }
}

#[test]
fn directory_records_stay_small() {
    assert_eq!(std::mem::size_of::<DirectoryData>(), 24);
    assert_eq!(std::mem::size_of::<Node>(), 6);
}
