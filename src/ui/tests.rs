use super::{AppView, Page};
use crate::models::{EntryKind, ScanEvent};

fn populated_view() -> AppView {
    let mut view = AppView::default();
    for (path, parent, kind, bytes) in [
        ("/root", None, EntryKind::Directory, 0),
        ("/root/nested", Some(0), EntryKind::Directory, 0),
        ("/root/top", Some(0), EntryKind::File, 200 * 1024 * 1024),
        (
            "/root/nested/large",
            Some(1),
            EntryKind::File,
            2 * 1024 * 1024 * 1024,
        ),
        ("/root/small", Some(0), EntryKind::File, 10),
    ] {
        view.results.apply(ScanEvent::Entry {
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
    view.views.entry(1).or_default().selected = Some(3);
    let directory_scroll = view.views[&1].scroll.0.clone();
    let large_scroll = view.large_file_view.scroll.0.clone();
    view.set_page(Page::LargeFiles);
    assert_eq!(view.visible, vec![3, 2]);
    assert_eq!(view.selected(), None);
    view.large_file_view.selected = Some(2);
    view.set_page(Page::Issues);
    assert!(view.visible.is_empty());
    assert_eq!(view.selected(), None);
    view.set_page(Page::Directory);
    assert_eq!(view.directory, 1);
    assert_eq!(view.visible, vec![3]);
    assert_eq!(view.selected(), Some(3));
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
    view.large_file_view.selected = Some(3);
    view.open_parent();
    assert!(view.page == Page::Directory);
    assert_eq!(view.directory, 1);
    assert_eq!(view.selected(), Some(3));
    assert_eq!(view.history, vec![0]);
    let previous = view.history.pop().unwrap();
    view.navigate_to(previous, false);
    assert_eq!(view.directory, 0);
    assert_eq!(view.selected(), Some(2));
    assert!(view.history.is_empty());
    view.set_page(Page::LargeFiles);
    assert_eq!(view.selected(), Some(3));
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
    assert_eq!(view.visible, vec![3]);
    assert_eq!(view.selected(), None);
    view.set_minimum(10 * 1024 * 1024 * 1024);
    assert!(view.visible.is_empty());
    view.set_minimum(0);
    assert_eq!(view.visible, vec![3, 2, 4]);
    view.set_page(Page::Directory);
    assert_eq!(view.selected(), Some(2));
}

#[test]
fn discoveries_while_issues_are_visible_are_indexed_when_returning_to_large_files() {
    let mut view = populated_view();
    view.set_page(Page::Issues);
    view.results.apply(ScanEvent::Entry {
        path: "/root/new".into(),
        parent: Some(0),
        kind: EntryKind::File,
        bytes: 3 * 1024 * 1024 * 1024,
    });
    view.large_files.update(&view.results);
    view.refresh_visible();
    assert!(view.visible.is_empty());
    view.set_page(Page::LargeFiles);
    assert_eq!(view.visible, vec![5, 3, 2]);
}

#[test]
fn batched_updates_keep_cached_directory_sorting_current_while_hidden() {
    let mut view = populated_view();
    view.set_page(Page::Issues);
    view.apply_updates([
        ScanEvent::Entry {
            path: "/root/new".into(),
            parent: Some(0),
            kind: EntryKind::File,
            bytes: 3 * 1024 * 1024 * 1024,
        },
        ScanEvent::Entry {
            path: "/root/nested/new".into(),
            parent: Some(1),
            kind: EntryKind::File,
            bytes: 4 * 1024 * 1024 * 1024,
        },
    ]);
    view.set_page(Page::Directory);
    assert_eq!(view.visible, vec![1, 5, 2, 4]);
    view.set_page(Page::LargeFiles);
    assert_eq!(view.visible, vec![6, 5, 3, 2]);
}

#[test]
fn throttled_results_refresh_without_waiting_for_another_worker_event() {
    let mut view = populated_view();
    view.results.state = crate::models::ScanState::Scanning;
    view.last_refresh = std::time::Instant::now();
    view.apply_updates([ScanEvent::Entry {
        path: "/root/new".into(),
        parent: Some(0),
        kind: EntryKind::File,
        bytes: 3 * 1024 * 1024 * 1024,
    }]);
    assert!(!view.refresh_if_due());
    assert!(!view.visible.contains(&5));
    view.last_refresh = std::time::Instant::now() - std::time::Duration::from_millis(101);
    assert!(view.refresh_if_due());
    assert_eq!(view.visible[0], 5);
    assert!(!view.visible_dirty);
    assert!(!view.refresh_if_due());
}

#[test]
fn finishing_flushes_throttled_visible_results_immediately() {
    let mut view = populated_view();
    view.results.state = crate::models::ScanState::Scanning;
    view.last_refresh = std::time::Instant::now();
    view.apply_updates([
        ScanEvent::Entry {
            path: "/root/new".into(),
            parent: Some(0),
            kind: EntryKind::File,
            bytes: 3 * 1024 * 1024 * 1024,
        },
        ScanEvent::Finished(crate::models::ScanState::Partial),
    ]);
    assert!(view.refresh_if_due());
    assert_eq!(view.visible[0], 5);
}
