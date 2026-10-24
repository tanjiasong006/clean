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
        assert_eq!(self.nodes.len(), results.nodes.len());
        for (id, node) in results.nodes.iter().enumerate() {
            assert_eq!(
                self.nodes[id],
                (
                    node.parent,
                    node.bytes,
                    node.pending_directories,
                    node.errors
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

fn sample_events() -> Vec<ScanEvent> {
    let mut events = vec![ScanEvent::Entry {
        hard_link_to: None,
        path: "/root".into(),
        parent: None,
        kind: EntryKind::Directory,
        bytes: 0,
    }];
    let mut directories = vec![0];
    let mut seed = 42_u64;
    for id in 1..2048 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let parent = directories[(seed >> 32) as usize % directories.len()];
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
                seed % 8192
            } else {
                0
            },
        });
        if kind == EntryKind::Directory {
            directories.push(id);
        }
        if id % 37 == 0 {
            events.push(ScanEvent::Issue {
                parent,
                issue: ScanIssue {
                    path: format!("/missing-{id}").into(),
                    message: "removed".into(),
                },
            });
        }
    }
    for id in directories {
        events.push(ScanEvent::DirectoryRead(id));
    }
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
            results
                .nodes
                .iter()
                .all(|node| node.pending_directories == 0)
        );
        assert!(results.nodes[0].errors > 0);
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
        bytes: 10,
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
    assert_eq!(results.nodes[0].bytes, 10240);
    assert_eq!(results.nodes[1].bytes, 10240);
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
    assert_eq!(results.nodes[0].pending_directories, 1);
    assert_eq!(results.nodes[0].errors, 1);
    assert!(!results.nodes[0].is_complete());
    assert!(!results.nodes[1].is_complete());
    assert!(!results.nodes[2].is_complete());
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
    assert_eq!(results.nodes[0].bytes, u64::MAX);
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
        for node in &results.nodes[..2] {
            assert!(node.size_overflow);
            assert!(!node.is_complete());
        }
        assert!(!results.nodes[2].size_overflow);
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
        assert_eq!(results.nodes[0].bytes, 512 + 4096 + 1024);
        assert_eq!(results.nodes[2].bytes, 1024);
        assert_eq!(results.nodes[3].bytes, 0);
        assert_eq!(results.nodes[3].hard_link_to, Some(1));
        assert_eq!(results.files, 2);
        assert_eq!(results.hard_links, 1);
        assert!(results.nodes.iter().all(|node| node.is_complete()));
        assert!(results.issues.is_empty());
    }
}
