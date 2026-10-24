use super::{AppView, Page};

fn visible(view: &AppView) -> Vec<usize> {
    (0..view.visible_len())
        .map(|index| view.visible_id(index))
        .collect()
}
use crate::models::{DiskSpace, EntryKind, ScanEvent, ScanResults, ScanState};

fn populated_view() -> AppView {
    let mut view = AppView::default();
    // Directory entries are emitted as contiguous runs: root's children first,
    // then the nested directory's children.
    for (path, parent, kind, bytes) in [
        ("/root", None, EntryKind::Directory, 0),
        ("/root/nested", Some(0), EntryKind::Directory, 0),
        ("/root/top", Some(0), EntryKind::File, 200 * 1024 * 1024),
        ("/root/small", Some(0), EntryKind::File, 10),
        (
            "/root/nested/large",
            Some(1),
            EntryKind::File,
            2 * 1024 * 1024 * 1024,
        ),
    ] {
        view.results.apply(ScanEvent::Entry {
            hard_link_to: None,
            path: path.into(),
            parent,
            kind,
            bytes,
        });
    }
    view.large_files.update(&view.results);
    view.refresh_visible();
    view
}

#[test]
fn switching_pages_keeps_independent_selections_and_current_directory() {
    let mut view = populated_view();
    view.navigate_to(1, true);
    view.views.entry(1).or_default().selected = Some(4);
    let directory_scroll = view.views[&1].scroll.0.clone();
    let large_scroll = view.large_file_view.scroll.0.clone();
    view.set_page(Page::LargeFiles);
    assert_eq!(visible(&view), vec![4, 2]);
    assert_eq!(view.selected(), None);
    view.large_file_view.selected = Some(2);
    view.set_page(Page::Issues);
    assert!(visible(&view).is_empty());
    assert_eq!(view.selected(), None);
    view.set_page(Page::Directory);
    assert_eq!(view.directory, 1);
    assert_eq!(visible(&view), vec![4]);
    assert_eq!(view.selected(), Some(4));
    view.set_page(Page::LargeFiles);
    assert_eq!(view.selected(), Some(2));
    assert!(std::rc::Rc::ptr_eq(
        &directory_scroll,
        &view.views[&1].scroll.0
    ));
    assert!(std::rc::Rc::ptr_eq(
        &large_scroll,
        &view.large_file_view.scroll.0
    ));
}

#[test]
fn parent_navigation_selects_file_and_keeps_back_navigation_selection() {
    let mut view = populated_view();
    view.views.entry(0).or_default().selected = Some(2);
    view.set_page(Page::LargeFiles);
    view.large_file_view.selected = Some(4);
    view.open_parent();
    assert!(view.page == Page::Directory);
    assert_eq!(view.directory, 1);
    assert_eq!(view.selected(), Some(4));
    assert_eq!(view.history, vec![0]);
    let previous = view.history.pop().unwrap();
    view.navigate_to(previous, false);
    assert_eq!(view.directory, 0);
    assert_eq!(view.selected(), Some(2));
    assert!(view.history.is_empty());
    view.set_page(Page::LargeFiles);
    assert_eq!(view.selected(), Some(4));
}

#[test]
fn filter_changes_clear_only_large_file_selection_and_unchanged_filter_keeps_it() {
    let mut view = populated_view();
    view.views.entry(0).or_default().selected = Some(2);
    view.set_page(Page::LargeFiles);
    view.large_file_view.selected = Some(2);
    view.set_minimum(100 * 1024 * 1024);
    assert_eq!(view.selected(), Some(2));
    view.set_minimum(1024 * 1024 * 1024);
    assert_eq!(visible(&view), vec![4]);
    assert_eq!(view.selected(), None);
    view.set_minimum(10 * 1024 * 1024 * 1024);
    assert!(visible(&view).is_empty());
    view.set_minimum(0);
    assert_eq!(visible(&view), vec![4, 2, 3]);
    view.set_page(Page::Directory);
    assert_eq!(view.selected(), Some(2));
}

#[test]
fn discoveries_while_issues_are_visible_are_indexed_when_returning_to_large_files() {
    let mut view = populated_view();
    view.set_page(Page::Issues);
    view.results.apply(ScanEvent::Entry {
        hard_link_to: None,
        path: "/root/nested/new".into(),
        parent: Some(1),
        kind: EntryKind::File,
        bytes: 3 * 1024 * 1024 * 1024,
    });
    view.large_files.update(&view.results);
    view.refresh_visible();
    assert!(visible(&view).is_empty());
    view.set_page(Page::LargeFiles);
    assert_eq!(visible(&view), vec![5, 4, 2]);
}

#[test]
fn batched_updates_keep_cached_directory_sorting_current_while_hidden() {
    let mut view = populated_view();
    view.navigate_to(1, false);
    view.set_page(Page::Issues);
    view.apply_updates([
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root/nested/a".into(),
            parent: Some(1),
            kind: EntryKind::File,
            bytes: 3 * 1024 * 1024 * 1024,
        },
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root/nested/b".into(),
            parent: Some(1),
            kind: EntryKind::File,
            bytes: 4 * 1024 * 1024 * 1024,
        },
    ]);
    view.set_page(Page::Directory);
    assert_eq!(visible(&view), vec![6, 5, 4]);
    view.set_page(Page::LargeFiles);
    assert_eq!(visible(&view), vec![6, 5, 4, 2]);
}

#[test]
fn throttled_results_refresh_without_waiting_for_another_worker_event() {
    let mut view = populated_view();
    view.navigate_to(1, false);
    view.results.state = ScanState::Scanning;
    view.last_refresh = std::time::Instant::now();
    view.apply_updates([ScanEvent::Entry {
        hard_link_to: None,
        path: "/root/nested/new".into(),
        parent: Some(1),
        kind: EntryKind::File,
        bytes: 3 * 1024 * 1024 * 1024,
    }]);
    assert!(!view.refresh_if_due());
    assert!(!visible(&view).contains(&5));
    view.last_refresh = std::time::Instant::now() - std::time::Duration::from_millis(101);
    assert!(view.refresh_if_due());
    assert_eq!(visible(&view)[0], 5);
    assert!(!view.visible_dirty);
    assert!(!view.refresh_if_due());
}

#[test]
fn finishing_flushes_throttled_visible_results_immediately() {
    let mut view = populated_view();
    view.navigate_to(1, false);
    view.results.state = ScanState::Scanning;
    view.last_refresh = std::time::Instant::now();
    view.apply_updates([
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root/nested/new".into(),
            parent: Some(1),
            kind: EntryKind::File,
            bytes: 3 * 1024 * 1024 * 1024,
        },
        ScanEvent::Finished(ScanState::Partial),
    ]);
    assert!(view.refresh_if_due());
    assert_eq!(visible(&view)[0], 5);
}

#[test]
fn capacity_notice_compares_allocated_total_to_capacity_not_available_space() {
    let mut view = populated_view();
    let bytes = view.results.bytes(0);
    view.disk_space = Some(Ok(DiskSpace {
        capacity: bytes + 1,
        available: 0,
    }));
    assert!(view.capacity_notice().is_none());
    view.disk_space = Some(Ok(DiskSpace {
        capacity: bytes,
        available: 0,
    }));
    assert!(view.capacity_notice().is_none());
    view.disk_space = Some(Ok(DiskSpace {
        capacity: bytes - 1,
        available: 0,
    }));
    assert!(view.capacity_notice().is_some());
    assert_eq!(view.results.bytes(0), bytes);
}

#[test]
fn capacity_notice_needs_a_known_capacity_and_an_exact_allocated_total() {
    let mut view = AppView::default();
    view.apply_updates([
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root".into(),
            parent: None,
            kind: EntryKind::Directory,
            bytes: 0,
        },
        ScanEvent::Entry {
            hard_link_to: None,
            path: "/root/huge".into(),
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
    ]);
    assert!(view.results.size_overflow(0));
    assert!(view.capacity_notice().is_none());
    view.disk_space = Some(Err("volume query failed".into()));
    assert!(view.capacity_notice().is_none());
    view.disk_space = Some(Ok(DiskSpace {
        capacity: 1,
        available: 0,
    }));
    assert!(view.capacity_notice().is_none());
    view.results = ScanResults::default();
    assert!(view.capacity_notice().is_none());
}

#[test]
fn hard_link_aliases_show_their_owner_instead_of_misleading_zero_bytes() {
    let mut view = populated_view();
    let before = view.results.bytes(0);
    view.apply_updates([ScanEvent::Entry {
        path: "/root/nested/alias".into(),
        parent: Some(1),
        kind: EntryKind::File,
        bytes: 0,
        hard_link_to: Some(2),
    }]);
    assert_eq!(view.results.bytes(0), before);
    assert_eq!(view.results.files, 4);
    assert_eq!(view.results.hard_links, 1);
    assert_eq!(view.entry_size(5), "已去重");
    assert_eq!(view.entry_description(5), "硬链接 · 已计入 /root/top");
    view.navigate_to(1, false);
    assert_eq!(visible(&view), vec![4, 5]);
    view.views.entry(1).or_default().selected = Some(5);
    assert_eq!(view.selected(), Some(5));
    view.set_minimum(0);
    view.set_page(Page::LargeFiles);
    assert_eq!(visible(&view), vec![4, 2, 3]);
}
