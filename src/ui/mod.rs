use std::{
    collections::HashMap,
    ops::Range,
    path::PathBuf,
    sync::{Arc, mpsc::TryRecvError},
    time::{Duration, Instant},
};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme, Icon, Sizable, Size, StyledExt, Theme,
    button::ButtonVariants,
    h_flex,
    menu::{ContextMenuExt, PopupMenuItem},
    scroll::ScrollableElement,
    spinner::Spinner,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use crate::{
    analysis::{Category, DirectoryIndex, LARGE_FILE_FLOOR, LargeFileIndex, format_bytes},
    models::{DiskSpace, EntryKind, NodeId, ScanEvent, ScanIssue, ScanResults, ScanState},
    platform,
    scanner::ScanJob,
};

mod appearance;
mod design;

use appearance::Appearance;
use design::{Palette, button, layout, radius, type_size};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Page {
    Overview,
    #[default]
    Directory,
    LargeFiles,
    Issues,
}

#[derive(Default)]
struct DirectoryView {
    selected: Option<NodeId>,
    scroll: UniformListScrollHandle,
}

#[derive(Clone)]
enum NavAction {
    Page(Page),
    Scan(PathBuf),
    Choose,
}

struct NavEntry {
    id: &'static str,
    icon: IconName,
    label: String,
    active: bool,
    disabled: bool,
    badge: Option<usize>,
    action: NavAction,
}

pub struct AppView {
    results: ScanResults,
    job: Option<ScanJob>,
    poll_task: Option<Task<()>>,
    directory: NodeId,
    views: HashMap<NodeId, DirectoryView>,
    history: Vec<NodeId>,
    page: Page,
    large_files: LargeFileIndex,
    directory_index: DirectoryIndex,
    last_refresh: Instant,
    visible_dirty: bool,
    large_file_view: DirectoryView,
    overview_view: DirectoryView,
    minimum_bytes: u64,
    disk_space: Option<Result<DiskSpace, String>>,
    disk_task: Option<Task<()>>,
    revealing: bool,
    issue_scroll: UniformListScrollHandle,
    cancelling: bool,
    choosing: bool,
    notice: Option<String>,
    scan_root: Option<PathBuf>,
    scan_started: Option<Instant>,
    scan_elapsed: Option<Duration>,
    last_clock: Instant,
    show_inspector: bool,
    focus_handle: Option<FocusHandle>,
    appearance: Appearance,
    appearance_subscription: Option<Subscription>,
}

impl Default for AppView {
    fn default() -> Self {
        Self {
            results: ScanResults::default(),
            job: None,
            poll_task: None,
            directory: 0,
            views: HashMap::new(),
            history: Vec::new(),
            page: Page::Directory,
            large_files: LargeFileIndex::default(),
            directory_index: DirectoryIndex::default(),
            last_refresh: Instant::now(),
            visible_dirty: false,
            large_file_view: DirectoryView::default(),
            overview_view: DirectoryView::default(),
            minimum_bytes: 100 * 1024 * 1024,
            disk_space: None,
            disk_task: None,
            revealing: false,
            issue_scroll: UniformListScrollHandle::new(),
            cancelling: false,
            choosing: false,
            notice: None,
            scan_root: None,
            scan_started: None,
            scan_elapsed: None,
            last_clock: Instant::now(),
            show_inspector: true,
            focus_handle: None,
            appearance: Appearance::default(),
            appearance_subscription: None,
        }
    }
}

impl AppView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            focus_handle: Some(cx.focus_handle()),
            ..Self::default()
        };
        match Appearance::load() {
            Ok(appearance) => view.appearance = appearance,
            Err(error) => view.notice = Some(format!("无法读取外观偏好：{error}")),
        }
        view.apply_appearance(window, cx);
        view.appearance_subscription =
            Some(cx.observe_window_appearance(window, |this, window, cx| {
                if this.appearance == Appearance::System {
                    this.apply_appearance(window, cx);
                }
            }));
        view
    }

    fn apply_appearance(&self, window: &mut Window, cx: &mut Context<Self>) {
        Theme::change(
            self.appearance.resolve(window.appearance().into()),
            Some(window),
            cx,
        );
        Theme::update(cx, design::apply);
    }

    fn set_appearance(
        &mut self,
        appearance: Appearance,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.appearance == appearance {
            return;
        }
        self.appearance = appearance;
        self.apply_appearance(window, cx);
        if let Err(error) = appearance.save() {
            self.notice = Some(format!("主题已切换，但无法保存外观偏好：{error}"));
        }
        cx.notify();
    }

    fn start_scan(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        // Keep at most one traversal alive. Wait for cancellation before a new scan.
        if self.job.is_some() {
            return;
        }
        let job = match ScanJob::start(path.clone()) {
            Ok(job) => job,
            Err(error) => {
                self.notice = Some(format!("无法启动扫描线程：{error}"));
                cx.notify();
                return;
            }
        };
        self.results = ScanResults::scanning();
        self.job = Some(job);
        self.scan_root = Some(path.clone());
        self.scan_started = Some(Instant::now());
        self.scan_elapsed = None;
        self.disk_space = None;
        self.disk_task = Some(cx.spawn(async move |this, cx| {
            let space = cx
                .background_executor()
                .spawn(
                    async move { platform::disk_space(&path).map_err(|error| error.to_string()) },
                )
                .await;
            let _ = this.update(cx, |this, cx| {
                this.disk_space = Some(space);
                cx.notify();
            });
        }));
        self.directory = 0;
        self.visible_dirty = false;
        self.views.clear();
        self.views.insert(0, DirectoryView::default());
        self.history.clear();
        self.large_files = LargeFileIndex::default();
        self.directory_index = DirectoryIndex::default();
        self.large_file_view = DirectoryView::default();
        self.overview_view = DirectoryView::default();
        self.page = Page::Overview;
        self.issue_scroll = UniformListScrollHandle::new();
        self.cancelling = false;
        self.notice = None;
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let keep_polling = this.update(cx, |this, cx| this.poll(cx));
                if !matches!(keep_polling, Ok(true)) {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn apply_updates(&mut self, events: impl IntoIterator<Item = ScanEvent>) {
        let changes = self.results.apply_batch(events);
        self.directory_index.update(&self.results, &changes);
        self.large_files.update(&self.results);
        self.visible_dirty = true;
    }

    fn refresh_if_due(&mut self) -> bool {
        if self.visible_dirty
            && (self.results.state != ScanState::Scanning
                || self.last_refresh.elapsed() >= Duration::from_millis(100))
        {
            self.refresh_visible();
            true
        } else {
            false
        }
    }

    fn capacity_notice(&self) -> Option<&'static str> {
        let space = self.disk_space.as_ref()?.as_ref().ok()?;
        (!self.results.is_empty()
            && !self.results.root_overflow()
            && self.results.root_bytes() > space.capacity)
            .then_some(
            "已分配空间总量超过所选卷容量；APFS 共享数据或跨卷扫描可能造成差异，此数值不代表可释放空间。",
        )
    }

    fn poll(&mut self, cx: &mut Context<Self>) -> bool {
        let started = Instant::now();
        let mut changed = false;
        // Bound both work and elapsed ingestion time. A fast disk is no longer
        // limited to 1024 events every 100ms, but cannot monopolize the UI.
        for _ in 0..32 {
            let Some(update) = self.job.as_ref().map(|job| job.updates.try_recv()) else {
                return false;
            };
            match update {
                Ok(batch) => {
                    self.apply_updates(batch);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.results.state == ScanState::Scanning {
                        self.results.state = if self.results.is_empty() {
                            ScanState::Failed
                        } else {
                            ScanState::Partial
                        };
                        self.apply_updates([ScanEvent::Issue {
                            parent: 0,
                            issue: ScanIssue {
                                path: self.scan_root.clone().unwrap_or_default(),
                                message: "扫描线程意外结束，已保留现有结果。".into(),
                            },
                        }]);
                        changed = true;
                    }
                    break;
                }
            }
            if started.elapsed() >= Duration::from_millis(4) {
                break;
            }
        }
        // A previous tick may have ingested results before the refresh
        // deadline. Publish them even if the worker is now waiting on I/O.
        changed |= self.refresh_if_due();
        if changed {
            cx.notify();
        }
        if self.results.state != ScanState::Scanning {
            if let Some(started) = self.scan_started.take() {
                self.scan_elapsed = Some(started.elapsed());
            }
            self.job = None;
            self.cancelling = false;
            return false;
        }
        // Keep the elapsed clock legible without repainting every frame.
        if self.last_clock.elapsed() >= Duration::from_millis(250) {
            self.last_clock = Instant::now();
            cx.notify();
        }
        true
    }

    fn choose_directory(&mut self, cx: &mut Context<Self>) {
        if self.choosing || self.job.is_some() {
            return;
        }
        self.choosing = true;
        cx.activate(true);
        let selection = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("选择扫描目录".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = selection.await;
            let _ = this.update(cx, |this, cx| {
                this.choosing = false;
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next() {
                            this.start_scan(path, cx);
                        }
                    }
                    Ok(Ok(None)) => {}
                    _ => this.notice = Some("无法打开目录选择器，请重试。".into()),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn navigate(&mut self, directory: NodeId, remember: bool, cx: &mut Context<Self>) {
        self.navigate_to(directory, remember);
        cx.notify();
    }

    fn navigate_to(&mut self, directory: NodeId, remember: bool) {
        if self.directory != directory {
            if remember {
                self.history.push(self.directory);
            }
            self.directory = directory;
            self.views.entry(directory).or_default();
        }
        self.set_page(Page::Directory);
    }

    fn refresh_visible(&mut self) {
        self.last_refresh = Instant::now();
        self.visible_dirty = false;
        match self.page {
            Page::Overview => {
                self.directory_index.select(&self.results, 0);
                self.directory_index.sync(&self.results);
                self.large_files.sync(&self.results, LARGE_FILE_FLOOR);
            }
            Page::Directory => {
                self.directory_index.select(&self.results, self.directory);
                self.directory_index.sync(&self.results);
            }
            Page::LargeFiles => self.large_files.sync(&self.results, self.minimum_bytes),
            Page::Issues => {}
        }
    }

    fn visible_len(&self) -> usize {
        match self.page {
            Page::Overview | Page::Directory => self.directory_index.len(),
            Page::LargeFiles => self
                .large_files
                .visible_len(&self.results, self.minimum_bytes),
            Page::Issues => 0,
        }
    }

    fn visible_id(&self, index: usize) -> NodeId {
        match self.page {
            Page::Overview | Page::Directory => self.directory_index.id(index),
            Page::LargeFiles => self.large_files.visible_id(self.minimum_bytes, index),
            Page::Issues => 0,
        }
    }

    fn show_page(&mut self, page: Page, cx: &mut Context<Self>) {
        self.set_page(page);
        cx.notify();
    }

    fn set_page(&mut self, page: Page) {
        self.page = page;
        self.refresh_visible();
    }

    fn set_minimum(&mut self, minimum: u64) {
        if self.minimum_bytes != minimum {
            self.minimum_bytes = minimum;
            self.large_file_view = DirectoryView::default();
            self.refresh_visible();
        }
    }

    fn open_parent(&mut self) {
        let Some(id) = self.selected() else { return };
        let Some(parent) = self.results.parent(id) else {
            return;
        };
        self.navigate_to(parent, true);
        self.views.entry(parent).or_default().selected = Some(id);
        if let Some(index) = (0..self.visible_len()).find(|index| self.visible_id(*index) == id) {
            self.views
                .entry(parent)
                .or_default()
                .scroll
                .scroll_to_item(index, ScrollStrategy::Center);
        }
    }

    fn selected(&self) -> Option<NodeId> {
        match self.page {
            Page::Overview => self.overview_view.selected,
            Page::Directory => self
                .views
                .get(&self.directory)
                .and_then(|view| view.selected),
            Page::LargeFiles => self.large_file_view.selected,
            Page::Issues => None,
        }
    }

    fn select_id(&mut self, id: NodeId) {
        match self.page {
            Page::Overview => self.overview_view.selected = Some(id),
            Page::Directory => self.views.entry(self.directory).or_default().selected = Some(id),
            Page::LargeFiles => self.large_file_view.selected = Some(id),
            Page::Issues => {}
        }
    }

    fn clear_selection(&mut self) {
        match self.page {
            Page::Overview => self.overview_view.selected = None,
            Page::Directory => self.views.entry(self.directory).or_default().selected = None,
            Page::LargeFiles => self.large_file_view.selected = None,
            Page::Issues => {}
        }
    }

    fn current_scroll(&self) -> Option<UniformListScrollHandle> {
        match self.page {
            Page::Overview => Some(self.overview_view.scroll.clone()),
            Page::Directory => Some(
                self.views
                    .get(&self.directory)
                    .map(|view| view.scroll.clone())
                    .unwrap_or_default(),
            ),
            Page::LargeFiles => Some(self.large_file_view.scroll.clone()),
            Page::Issues => None,
        }
    }

    fn focus_list(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handle) = &self.focus_handle {
            handle.focus(window, cx);
        }
    }

    fn select_step(&mut self, delta: isize) -> Option<usize> {
        let len = self.visible_len();
        if len == 0 {
            return None;
        }
        let current = self
            .selected()
            .and_then(|selected| (0..len).find(|index| self.visible_id(*index) == selected));
        let next = match current {
            Some(index) => (index as isize + delta).clamp(0, len as isize - 1) as usize,
            None if delta >= 0 => 0,
            None => len - 1,
        };
        let id = self.visible_id(next);
        self.select_id(id);
        Some(next)
    }

    fn step_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        if let Some(next) = self.select_step(delta) {
            if let Some(scroll) = self.current_scroll() {
                scroll.scroll_to_item(next, ScrollStrategy::Center);
            }
            cx.notify();
        }
    }

    fn enter_selected(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected() else { return };
        if self.results.kind(id) == EntryKind::Directory {
            self.navigate(id, true, cx);
            return;
        }
        if self.page == Page::LargeFiles {
            self.open_parent();
            cx.notify();
            return;
        }
        self.reveal_selected(cx);
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        if self.page != Page::Directory {
            self.set_page(Page::Directory);
            cx.notify();
            return;
        }
        if let Some(id) = self.history.pop() {
            self.navigate_to(id, false);
            cx.notify();
        }
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if self.page == Page::LargeFiles {
            self.open_parent();
            cx.notify();
            return;
        }
        if self.page != Page::Directory {
            return;
        }
        let Some(parent) = self.results.parent(self.directory) else {
            return;
        };
        self.navigate(parent, true, cx);
    }

    fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;

        if modifiers.platform && !modifiers.shift && key == "c" {
            self.copy_selected(cx);
            return;
        }
        if modifiers.platform && !modifiers.shift && key == "i" {
            self.show_inspector = !self.show_inspector;
            cx.notify();
            return;
        }
        if modifiers.platform && modifiers.shift && key == "r" {
            self.reveal_selected(cx);
            return;
        }
        if modifiers.platform && !modifiers.shift && key == "up" {
            self.go_up(cx);
            return;
        }
        if modifiers.modified() {
            return;
        }

        match key {
            "up" => self.step_selection(-1, cx),
            "down" => self.step_selection(1, cx),
            "enter" => self.enter_selected(cx),
            "backspace" => self.go_back(cx),
            "escape" => {
                self.clear_selection();
                cx.notify();
            }
            _ => {}
        }
    }

    fn copy_selected(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected() else { return };
        let path = self.results.to_path_buf(id);
        cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()));
    }

    fn reveal_selected(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected() else { return };
        if self.revealing {
            return;
        }
        let path = self.results.to_path_buf(id);
        self.revealing = true;
        self.notice = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { platform::reveal_item(&path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.revealing = false;
                if let Err(error) = result {
                    this.notice = Some(platform::reveal_error(error));
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn denominator(&self) -> NodeId {
        match self.page {
            Page::Directory => self.directory,
            _ => 0,
        }
    }

    fn share(&self, id: NodeId) -> Option<f32> {
        if self.page == Page::Issues || self.results.is_empty() {
            return None;
        }
        if self.results.hard_link_to(id).is_some() {
            return None;
        }
        let denominator = self.denominator();
        if denominator >= self.results.len() || self.results.size_overflow(denominator) {
            return None;
        }
        let total = self.results.bytes(denominator);
        if total == 0 {
            return None;
        }
        if id == denominator {
            return Some(1.0);
        }
        Some((self.results.bytes(id) as f64 / total as f64) as f32)
    }

    fn share_label(&self, id: NodeId) -> String {
        match self.share(id) {
            Some(ratio) if ratio > 0.0 && ratio < 0.001 => "<0.1%".into(),
            Some(ratio) => format!("{:.1}%", ratio * 100.0),
            None if self.results.hard_link_to(id).is_some() => "已去重".into(),
            None => "—".into(),
        }
    }

    fn share_caption(&self) -> &'static str {
        match self.page {
            Page::Directory => "占当前目录",
            Page::LargeFiles => "占扫描总量",
            Page::Overview => "占扫描总量",
            Page::Issues => "占比",
        }
    }

    #[cfg(test)]
    fn entry_description(&self, id: NodeId) -> String {
        if let Some(owner) = self.results.hard_link_to(id) {
            return format!("硬链接 · 已计入 {}", self.results.path(owner).display());
        }
        if self.page == Page::LargeFiles {
            return self.results.path(id).display().to_string();
        }
        let status = match self.results.kind(id) {
            EntryKind::Symlink => "符号链接 · 已跳过",
            EntryKind::Other => "特殊文件 · 已跳过",
            EntryKind::Directory if self.results.size_overflow(id) => "已分配空间超出统计范围",
            EntryKind::Directory if self.results.errors(id) > 0 => "含读取失败项",
            EntryKind::Directory if !self.results.is_complete(id) => "尚未扫描完整",
            EntryKind::Directory if self.results.child_count(id) == 0 => "空目录",
            EntryKind::Directory => "目录 · 双击进入",
            EntryKind::File => "文件",
        };
        status.to_string()
    }

    fn entry_size(&self, id: NodeId) -> String {
        if self.results.hard_link_to(id).is_some() {
            "已去重".into()
        } else if self.results.size_overflow(id) {
            "超出范围".into()
        } else {
            format_bytes(self.results.bytes(id))
        }
    }

    fn row_annotation(&self, id: NodeId, cx: &App) -> Option<(String, Hsla)> {
        let palette = Palette::for_theme(cx.theme());
        match self.results.kind(id) {
            EntryKind::Directory if self.results.size_overflow(id) => {
                Some(("大小超出范围".into(), palette.error))
            }
            EntryKind::Directory if self.results.errors(id) > 0 => Some((
                format!("{} 项读取失败", format_count(self.results.errors(id))),
                palette.error,
            )),
            EntryKind::Directory
                if !self.results.is_complete(id) && self.results.state != ScanState::Scanning =>
            {
                Some(("未扫描完整".into(), palette.warning))
            }
            EntryKind::Directory if self.results.child_count(id) == 0 => {
                Some(("空目录".into(), palette.text_tertiary))
            }
            EntryKind::Symlink | EntryKind::Other => Some(("未计入".into(), palette.text_tertiary)),
            _ => None,
        }
    }

    fn kind_label(&self, id: NodeId) -> &'static str {
        match self.results.kind(id) {
            EntryKind::Directory => "目录",
            EntryKind::File if self.results.hard_link_to(id).is_some() => "文件（硬链接别名）",
            EntryKind::File => "文件",
            EntryKind::Symlink => "符号链接",
            EntryKind::Other => "特殊文件",
        }
    }

    fn entry_icon(&self, id: NodeId, selected: bool, cx: &App) -> Icon {
        let palette = Palette::for_theme(cx.theme());
        let kind = self.results.kind(id);
        let error = kind == EntryKind::Directory && self.results.errors(id) > 0;
        let color = if error {
            palette.error
        } else if selected {
            palette.data_accent
        } else if kind == EntryKind::Directory {
            palette.folder
        } else {
            palette.text_tertiary
        };
        let name = match kind {
            EntryKind::Directory => IconName::Folder,
            EntryKind::File => IconName::File,
            EntryKind::Symlink => IconName::Link2,
            EntryKind::Other => IconName::FileQuestionMark,
        };
        Icon::new(name).with_size(px(15.0)).text_color(color)
    }

    fn root_label(&self) -> String {
        let Some(root) = &self.scan_root else {
            return "尚未选择扫描位置".into();
        };
        if platform::home_directory().as_deref() == Some(root.as_path()) {
            return "主目录".into();
        }
        root.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string())
    }

    fn elapsed_label(&self) -> Option<String> {
        let duration = match self.scan_started {
            Some(started) => started.elapsed(),
            None => self.scan_elapsed?,
        };
        Some(duration_text(duration))
    }

    fn state_label(&self) -> &'static str {
        if self.cancelling {
            return "正在停止";
        }
        match self.results.state {
            ScanState::Idle => "未开始",
            ScanState::Scanning => "扫描中",
            ScanState::Partial => "不完整",
            ScanState::Complete => "已完成",
            ScanState::Failed => "失败",
        }
    }

    fn state_color(&self, cx: &App) -> Hsla {
        match self.results.state {
            ScanState::Scanning => cx.theme().chart_2,
            ScanState::Partial => cx.theme().yellow,
            ScanState::Failed => cx.theme().red,
            _ => cx.theme().muted_foreground,
        }
    }

    fn sidebar_entries(&self) -> Vec<NavEntry> {
        let has_scan = self.scan_root.is_some();
        let scanning = self.results.state == ScanState::Scanning;
        let home = platform::home_directory();
        let downloads = platform::downloads_directory();
        let scanning_home = home.is_some() && self.scan_root == home;
        let scanning_downloads = downloads.is_some() && self.scan_root == downloads;
        let mut entries = vec![
            NavEntry {
                id: "nav-overview",
                icon: IconName::LayoutDashboard,
                label: "概览".into(),
                active: self.page == Page::Overview,
                disabled: !has_scan,
                badge: None,
                action: NavAction::Page(Page::Overview),
            },
            NavEntry {
                id: "nav-directory",
                icon: IconName::FolderTree,
                label: "目录".into(),
                active: self.page == Page::Directory,
                disabled: !has_scan,
                badge: None,
                action: NavAction::Page(Page::Directory),
            },
            NavEntry {
                id: "nav-large-files",
                icon: IconName::Files,
                label: "大文件".into(),
                active: self.page == Page::LargeFiles,
                disabled: !has_scan,
                badge: None,
                action: NavAction::Page(Page::LargeFiles),
            },
            NavEntry {
                id: "nav-issues",
                icon: IconName::TriangleAlert,
                label: "扫描问题".into(),
                active: self.page == Page::Issues,
                disabled: !has_scan,
                badge: (!self.results.issues.is_empty()).then_some(self.results.issues.len()),
                action: NavAction::Page(Page::Issues),
            },
        ];

        if let Some(home) = home {
            entries.push(NavEntry {
                id: "scan-home",
                icon: IconName::House,
                label: "主目录".into(),
                active: scanning_home && scanning,
                disabled: scanning,
                badge: None,
                action: NavAction::Scan(home),
            });
        }
        if let Some(downloads) = downloads {
            entries.push(NavEntry {
                id: "scan-downloads",
                icon: IconName::Download,
                label: "下载".into(),
                active: scanning_downloads && scanning,
                disabled: scanning,
                badge: None,
                action: NavAction::Scan(downloads),
            });
        }
        entries.push(NavEntry {
            id: "scan-choose",
            icon: IconName::FolderOpen,
            label: "选择目录…".into(),
            active: false,
            disabled: scanning || self.choosing,
            badge: None,
            action: NavAction::Choose,
        });
        entries
    }

    fn render_sidebar_entry(&self, entry: NavEntry, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let palette = Palette::for_theme(&theme);
        let active = entry.active && !entry.disabled;
        let disabled = entry.disabled;
        let badge = entry.badge;
        let action = entry.action.clone();
        let text_color = if disabled {
            palette.text_disabled
        } else if active {
            theme.sidebar_accent_foreground
        } else {
            theme.sidebar_foreground
        };

        div()
            .id(entry.id)
            .w_full()
            .h(px(layout::NAV_ROW))
            .px_2()
            .rounded(px(radius::CONTROL))
            .flex()
            .items_center()
            .gap_2()
            .bg(if active {
                theme.sidebar_accent
            } else {
                theme.transparent
            })
            .text_size(px(type_size::BODY))
            .font_medium()
            .text_color(text_color)
            .aria_label(entry.label.clone())
            .when(!disabled, |this| {
                let action = action.clone();
                this.cursor_pointer()
                    .hover(|style| style.bg(palette.hover))
                    .active(|style| style.bg(palette.pressed))
                    .on_click(cx.listener(move |this, _, _, cx| match &action {
                        NavAction::Page(page) => this.show_page(*page, cx),
                        NavAction::Scan(path) => this.start_scan(path.clone(), cx),
                        NavAction::Choose => this.choose_directory(cx),
                    }))
            })
            .child(
                Icon::new(entry.icon)
                    .with_size(px(14.0))
                    .text_color(if disabled {
                        palette.text_disabled
                    } else if active {
                        theme.chart_2
                    } else {
                        theme.muted_foreground
                    }),
            )
            .child(div().flex_1().min_w_0().truncate().child(entry.label))
            .children(badge.map(|count| {
                div()
                    .h(px(16.0))
                    .px_1()
                    .rounded_full()
                    .bg(theme.muted)
                    .text_size(px(type_size::CAPTION))
                    .text_color(palette.text_tertiary)
                    .child(format_count(count))
            }))
            .into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let palette = Palette::for_theme(&theme);
        let entries = self.sidebar_entries();
        let (analysis, scans): (Vec<_>, Vec<_>) = entries
            .into_iter()
            .partition(|entry| matches!(entry.action, NavAction::Page(_)));

        v_flex()
            .w(px(layout::SIDEBAR))
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(palette.border)
            .text_color(theme.sidebar_foreground)
            .child(
                h_flex()
                    .px_3()
                    .pt_4()
                    .pb_3()
                    .gap_2()
                    .child(
                        div()
                            .size(px(28.0))
                            .rounded(px(radius::CONTROL))
                            .bg(theme.primary)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                Icon::new(IconName::HardDrive)
                                    .with_size(px(16.0))
                                    .text_color(theme.primary_foreground),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_0()
                            .child(
                                div()
                                    .text_size(px(type_size::HEADING))
                                    .font_semibold()
                                    .child("Clean"),
                            )
                            .child(
                                div()
                                    .text_size(px(type_size::CAPTION))
                                    .text_color(palette.text_tertiary)
                                    .child("磁盘空间分析"),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .px_2()
                    .gap_1()
                    .child(section_label("分析", palette.text_tertiary))
                    .children(
                        analysis
                            .into_iter()
                            .map(|entry| self.render_sidebar_entry(entry, cx)),
                    ),
            )
            .child(
                v_flex()
                    .px_2()
                    .pt_4()
                    .gap_1()
                    .child(section_label("扫描位置", palette.text_tertiary))
                    .children(
                        scans
                            .into_iter()
                            .map(|entry| self.render_sidebar_entry(entry, cx)),
                    ),
            )
            .child(div().flex_1())
            .child(
                v_flex()
                    .px_3()
                    .py_3()
                    .gap_2()
                    .child(section_label("外观", palette.text_tertiary))
                    .child(
                        h_flex().gap_1().children(
                            [Appearance::System, Appearance::Light, Appearance::Dark]
                                .into_iter()
                                .map(|appearance| {
                                    let selected = self.appearance == appearance;
                                    button(appearance.name(), false, &theme)
                                        .with_size(Size::Small)
                                        .label(appearance.label())
                                        .when(selected, |button| button.secondary())
                                        .when(!selected, |button| button.ghost())
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.set_appearance(appearance, window, cx);
                                        }))
                                }),
                        ),
                    ),
            )
            .child(
                v_flex()
                    .px_3()
                    .py_3()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(type_size::LABEL))
                            .text_color(theme.sidebar_foreground)
                            .truncate()
                            .child(self.root_label()),
                    )
                    .child(
                        div()
                            .text_size(px(type_size::CAPTION))
                            .text_color(palette.text_tertiary)
                            .child("只读分析 · 不修改文件"),
                    ),
            )
            .into_any_element()
    }

    fn render_topbar(&self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let palette = Palette::for_theme(&theme);
        let scanning = self.results.state == ScanState::Scanning;
        let root = self
            .scan_root
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "选择位置开始分析".into());

        let action = if scanning {
            button("stop-scan", self.cancelling, &theme)
                .secondary()
                .with_size(Size::Small)
                .label(if self.cancelling {
                    "正在停止…"
                } else {
                    "停止扫描"
                })
                .icon(IconName::Square)
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(job) = &this.job {
                        job.cancel();
                        this.cancelling = true;
                        cx.notify();
                    }
                }))
                .into_any_element()
        } else {
            let root_path = self.scan_root.clone();
            button("rescan", root_path.is_none() || self.choosing, &theme)
                .secondary()
                .with_size(Size::Small)
                .label("重新扫描")
                .icon(IconName::RotateCw)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(path) = &root_path {
                        if let Some(job) = &this.job {
                            job.cancel();
                        }
                        let path = path.clone();
                        this.start_scan(path, cx);
                    }
                }))
                .into_any_element()
        };

        h_flex()
            .h(px(layout::TOOLBAR))
            .bg(palette.toolbar)
            .border_b_1()
            .border_color(palette.border)
            .px_4()
            .gap_3()
            .child(
                Icon::new(IconName::HardDrive)
                    .with_size(px(16.0))
                    .text_color(theme.muted_foreground),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0()
                    .child(
                        div()
                            .text_size(px(type_size::CAPTION))
                            .text_color(palette.text_tertiary)
                            .child("扫描位置"),
                    )
                    .child(div().text_size(px(type_size::BODY)).truncate().child(root)),
            )
            .when(!compact, |this| {
                this.child(
                    button("choose-directory", scanning || self.choosing, &theme)
                        .with_size(Size::Small)
                        .label("选择目录…")
                        .icon(IconName::FolderOpen)
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| this.choose_directory(cx))),
                )
            })
            .child(action)
            .when(self.page != Page::Issues, |this| {
                this.child(
                    button("toggle-inspector", false, &theme)
                        .with_size(Size::Small)
                        .icon(IconName::Info)
                        .ghost()
                        .tooltip(if self.show_inspector {
                            "隐藏详情"
                        } else {
                            "显示详情"
                        })
                        .toggled(self.show_inspector)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_inspector = !this.show_inspector;
                            cx.notify();
                        })),
                )
            })
            .into_any_element()
    }

    fn render_summary(&self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let palette = Palette::for_theme(&theme);
        let total = if self.results.is_empty() {
            "0 B".to_string()
        } else if self.results.root_overflow() {
            "超出统计范围".to_string()
        } else {
            format_bytes(self.results.root_bytes())
        };
        let scanning = self.results.state == ScanState::Scanning;
        let state_color = self.state_color(cx);

        let mut stats = vec![
            ("文件", format_count(self.results.files)),
            ("目录", format_count(self.results.directories)),
        ];
        if !compact {
            stats.push(("硬链接", format_count(self.results.hard_links)));
            stats.push(("用时", self.elapsed_label().unwrap_or_else(|| "—".into())));
        }

        let volume = if compact {
            None
        } else {
            Some(match &self.disk_space {
                Some(Ok(space)) => {
                    let used = space.capacity.saturating_sub(space.available);
                    let ratio = if space.capacity == 0 {
                        0.0
                    } else {
                        (used as f64 / space.capacity as f64) as f32
                    };
                    v_flex()
                        .w(px(220.0))
                        .gap_1()
                        .child(
                            h_flex()
                                .justify_between()
                                .gap_3()
                                .text_size(px(type_size::CAPTION))
                                .child(div().text_color(palette.text_tertiary).child("所选卷"))
                                .child(
                                    div()
                                        .text_color(palette.text_secondary)
                                        .font_features(tabular_numbers())
                                        .child(format!(
                                            "可用 {} / {}",
                                            format_bytes(space.available),
                                            format_bytes(space.capacity)
                                        )),
                                ),
                        )
                        .child(
                            div()
                                .w_full()
                                .h(px(4.0))
                                .rounded_full()
                                .bg(palette.bar_track)
                                .child(
                                    div()
                                        .h_full()
                                        .w(relative(ratio.clamp(0.0, 1.0)))
                                        .rounded_full()
                                        .bg(palette.meter),
                                ),
                        )
                        .into_any_element()
                }
                Some(Err(error)) => div()
                    .w(px(220.0))
                    .text_size(px(type_size::CAPTION))
                    .text_color(palette.text_tertiary)
                    .child(format!("无法读取所选卷容量：{error}"))
                    .into_any_element(),
                None => div()
                    .w(px(220.0))
                    .text_size(px(type_size::CAPTION))
                    .text_color(palette.text_tertiary)
                    .child("正在查询所选卷容量…")
                    .into_any_element(),
            })
        };

        v_flex()
            .bg(palette.recessed)
            .px_4()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(palette.border)
            .child(
                h_flex()
                    .items_center()
                    .gap_4()
                    .child(
                        v_flex()
                            .min_w(px(168.0))
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(type_size::CAPTION))
                                    .text_color(palette.text_tertiary)
                                    .child("已分配空间"),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(
                                        div()
                                            .text_size(px(type_size::DISPLAY))
                                            .font_semibold()
                                            .font_features(tabular_numbers())
                                            .child(total),
                                    )
                                    .child(
                                        h_flex()
                                            .id("scan-state")
                                            .gap_1()
                                            .items_center()
                                            .aria_label(self.results.state.label())
                                            .child(if scanning {
                                                Spinner::new()
                                                    .with_size(px(10.0))
                                                    .color(state_color)
                                                    .into_any_element()
                                            } else {
                                                div()
                                                    .size(px(6.0))
                                                    .rounded_full()
                                                    .bg(state_color)
                                                    .into_any_element()
                                            })
                                            .child(
                                                div()
                                                    .text_size(px(type_size::CAPTION))
                                                    .text_color(palette.text_tertiary)
                                                    .child(self.state_label()),
                                            ),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .pl_5()
                            .border_l_1()
                            .border_color(palette.border)
                            .gap_5()
                            .items_center()
                            .children(
                                stats
                                    .into_iter()
                                    .map(|(label, value)| metric(label, value, &theme)),
                            ),
                    )
                    .child(div().flex_1())
                    .children(volume),
            )
            .children(self.capacity_notice().map(|notice| {
                h_flex()
                    .gap_2()
                    .text_size(px(type_size::CAPTION))
                    .text_color(palette.text_tertiary)
                    .child(
                        Icon::new(IconName::TriangleAlert)
                            .with_size(px(12.0))
                            .text_color(theme.yellow),
                    )
                    .child(notice)
            }))
            .children(self.notice.clone().map(|notice| {
                h_flex()
                    .gap_2()
                    .text_size(px(type_size::CAPTION))
                    .text_color(theme.red)
                    .child(
                        Icon::new(IconName::CircleAlert)
                            .with_size(px(12.0))
                            .text_color(theme.red),
                    )
                    .child(notice)
            }))
            .into_any_element()
    }

    fn render_breadcrumbs(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let palette = Palette::for_theme(&theme);
        let mut ancestors = Vec::new();
        let mut ancestor = (self.directory < self.results.len()).then_some(self.directory);
        while let Some(id) = ancestor {
            ancestors.push(id);
            ancestor = self.results.parent(id);
        }
        ancestors.reverse();

        let mut crumbs = Vec::new();
        for (index, id) in ancestors.into_iter().enumerate() {
            if index > 0 {
                crumbs.push(
                    Icon::new(IconName::ChevronRight)
                        .with_size(px(10.0))
                        .text_color(palette.text_tertiary)
                        .into_any_element(),
                );
            }
            let label = if id == 0 {
                self.root_label()
            } else {
                self.results.name(id).to_string_lossy().into_owned()
            };
            let active = id == self.directory;
            crumbs.push(
                div()
                    .id(ElementId::named_usize("crumb", id))
                    .px_1()
                    .py_1()
                    .rounded(px(radius::SMALL))
                    .text_size(px(type_size::BODY))
                    .text_color(if active {
                        theme.foreground
                    } else {
                        theme.muted_foreground
                    })
                    .when(!active, |this| {
                        this.cursor_pointer()
                            .hover(|style| style.bg(theme.accent))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.navigate(id, true, cx)),
                            )
                    })
                    .child(label)
                    .into_any_element(),
            );
        }

        h_flex()
            .id("breadcrumbs")
            .flex_1()
            .min_w_0()
            .gap_1()
            .overflow_x_scroll()
            .children(crumbs)
            .into_any_element()
    }

    fn render_content_toolbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let palette = Palette::for_theme(&theme);
        h_flex()
            .h(px(layout::CONTENT_TOOLBAR))
            .bg(palette.toolbar)
            .px_3()
            .gap_2()
            .child(
                button("back", self.history.is_empty(), &theme)
                    .with_size(Size::Small)
                    .icon(IconName::ChevronLeft)
                    .ghost()
                    .tooltip(format!("返回 ({})", platform::BACK_SHORTCUT))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(id) = this.history.pop() {
                            this.navigate_to(id, false);
                            cx.notify();
                        }
                    })),
            )
            .child(self.render_breadcrumbs(cx))
            .child(
                div()
                    .text_size(px(type_size::CAPTION))
                    .text_color(palette.text_tertiary)
                    .child("按大小 ↓"),
            )
            .child(
                button(
                    "reveal-toolbar",
                    self.selected().is_none() || self.revealing,
                    &theme,
                )
                .with_size(Size::Small)
                .icon(IconName::FolderSearch)
                .ghost()
                .tooltip(format!("{}所选项目", platform::reveal_label()))
                .on_click(cx.listener(|this, _, _, cx| this.reveal_selected(cx))),
            )
            .into_any_element()
    }

    fn render_table_header(&self, wide: bool, show_children: bool, cx: &App) -> AnyElement {
        let theme = cx.theme().clone();
        let palette = Palette::for_theme(&theme);
        h_flex()
            .h(px(layout::TABLE_HEAD))
            .border_b_1()
            .border_color(palette.border)
            .bg(palette.recessed)
            .px_3()
            .gap_3()
            .text_size(px(type_size::CAPTION))
            .text_color(palette.text_tertiary)
            .child(div().flex_1().min_w_0().pl(px(27.0)).child("名称"))
            .when(self.page == Page::LargeFiles && wide, |this| {
                this.child(div().w(px(200.0)).child("位置"))
            })
            .when(
                self.page != Page::LargeFiles && wide && show_children,
                |this| this.child(div().w(px(84.0)).text_right().child("直接子项")),
            )
            .child(div().w(px(150.0)).child("占比"))
            .child(div().w(px(92.0)).text_right().child("大小"))
            .into_any_element()
    }

    fn render_row(&self, index: usize, wide: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let palette = Palette::for_theme(&theme);
        let id = self.visible_id(index);
        let selected = self.selected() == Some(id);
        let kind = self.results.kind(id);
        let is_directory = kind == EntryKind::Directory;
        let page = self.page;
        let name = self.results.name(id).to_string_lossy().into_owned();
        let annotation = self.row_annotation(id, cx);
        let share = self.share(id);
        let entity = cx.entity().downgrade();
        let path = self.results.to_path_buf(id);
        let location = (page == Page::LargeFiles && wide)
            .then(|| self.results.parent(id))
            .flatten()
            .map(|parent| self.results.path(parent).display().to_string());
        let name_color = if selected {
            palette.on_selected
        } else {
            palette.text_primary
        };
        let detail_color = if selected {
            palette.on_selected.opacity(0.8)
        } else {
            palette.text_tertiary
        };

        let row = div()
            .id(ElementId::named_usize(
                if page == Page::Overview {
                    "overview-row"
                } else {
                    "row"
                },
                id,
            ))
            .relative()
            .w_full()
            .h(px(layout::ROW))
            .px_3()
            .gap_3()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(palette.border)
            .bg(if selected {
                theme.list_active
            } else {
                theme.transparent
            })
            .when(!selected, |this| {
                this.hover(|style| style.bg(theme.list_hover))
            })
            .cursor_pointer()
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.focus_list(window, cx);
                this.select_id(id);
                if is_directory && event.click_count() == 2 {
                    this.navigate(id, true, cx);
                } else if page == Page::LargeFiles && event.click_count() == 2 {
                    this.open_parent();
                }
                cx.notify();
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, window, cx| {
                    this.focus_list(window, cx);
                    this.select_id(id);
                    cx.notify();
                }),
            )
            .children(selected.then(|| {
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(2.0))
                    .bg(palette.selected_border)
            }))
            .child(self.entry_icon(id, selected, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(type_size::BODY))
                            .text_color(name_color)
                            .child(name),
                    )
                    .children(annotation.map(|(text, color)| {
                        div()
                            .flex_shrink_0()
                            .text_size(px(type_size::CAPTION))
                            .text_color(if selected { detail_color } else { color })
                            .child(text)
                    })),
            )
            .when(page != Page::LargeFiles && wide, |this| {
                this.child(
                    div()
                        .w(px(84.0))
                        .text_right()
                        .text_size(px(type_size::CAPTION))
                        .text_color(detail_color)
                        .font_features(tabular_numbers())
                        .child(self.child_count_label(id)),
                )
            })
            .children(location.map(|location| {
                div()
                    .w(px(200.0))
                    .min_w_0()
                    .truncate()
                    .text_size(px(type_size::CAPTION))
                    .text_color(detail_color)
                    .child(location)
            }))
            .child(
                h_flex()
                    .w(px(150.0))
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .h(px(4.0))
                            .rounded_full()
                            .bg(palette.bar_track)
                            .child(match share {
                                Some(ratio) => div()
                                    .h_full()
                                    .w(relative(ratio.clamp(0.0, 1.0)))
                                    .rounded_full()
                                    .bg(palette.bar_fill)
                                    .into_any_element(),
                                None => div().h_full().w(px(0.0)).into_any_element(),
                            }),
                    )
                    .child(
                        div()
                            .w(px(46.0))
                            .text_right()
                            .text_size(px(type_size::CAPTION))
                            .text_color(if selected {
                                detail_color
                            } else {
                                palette.text_secondary
                            })
                            .font_features(tabular_numbers())
                            .child(self.share_label(id)),
                    ),
            )
            .child(
                div()
                    .w(px(92.0))
                    .text_right()
                    .text_size(px(type_size::BODY))
                    .font_medium()
                    .font_features(tabular_numbers())
                    .text_color(if selected {
                        palette.on_selected
                    } else {
                        palette.text_primary
                    })
                    .child(self.entry_size(id)),
            );

        row.context_menu(move |menu, _, _| {
            let mut menu = menu;
            if is_directory && page != Page::Issues {
                let entity = entity.clone();
                menu = menu.item(
                    PopupMenuItem::new("进入目录")
                        .icon(IconName::FolderOpen)
                        .on_click(move |_, _, cx| {
                            let _ = entity.update(cx, |this, cx| {
                                this.navigate(id, true, cx);
                            });
                        }),
                );
            }
            let reveal = entity.clone();
            let reveal_path = path.clone();
            menu = menu.item(
                PopupMenuItem::new(platform::reveal_label())
                    .icon(IconName::FolderSearch)
                    .on_click(move |_, _, cx| {
                        let path = reveal_path.clone();
                        let _ = reveal.update(cx, |this, cx| {
                            this.find_in_finder(path, cx);
                        });
                    }),
            );
            let copy_path = path.clone();
            menu = menu.item(
                PopupMenuItem::new("复制路径")
                    .icon(IconName::Copy)
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            copy_path.display().to_string(),
                        ));
                    }),
            );
            if page == Page::LargeFiles {
                let entity = entity.clone();
                menu = menu.separator().item(
                    PopupMenuItem::new("前往所在目录")
                        .icon(IconName::CornerDownRight)
                        .on_click(move |_, _, cx| {
                            let _ = entity.update(cx, |this, cx| {
                                this.open_parent();
                                cx.notify();
                            });
                        }),
                );
            }
            menu
        })
        .into_any_element()
    }

    fn child_count_label(&self, id: NodeId) -> String {
        match self.results.kind(id) {
            EntryKind::Directory => format_count(self.results.child_count(id)),
            _ => String::new(),
        }
    }

    fn find_in_finder(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.revealing {
            return;
        }
        self.revealing = true;
        self.notice = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { platform::reveal_item(&path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.revealing = false;
                if let Err(error) = result {
                    this.notice = Some(platform::reveal_error(error));
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn render_empty_state(
        &self,
        title: &str,
        detail: &str,
        icon: IconName,
        scanning: bool,
        cx: &App,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_2()
            .p_8()
            .child(if scanning {
                Spinner::new()
                    .with_size(px(18.0))
                    .color(theme.chart_2)
                    .into_any_element()
            } else {
                Icon::new(icon)
                    .with_size(px(22.0))
                    .text_color(theme.muted_foreground)
                    .into_any_element()
            })
            .child(
                div()
                    .text_size(px(type_size::HEADING))
                    .font_medium()
                    .child(title.to_string()),
            )
            .child(
                div()
                    .max_w(px(420.0))
                    .text_center()
                    .text_size(px(type_size::LABEL))
                    .text_color(theme.muted_foreground)
                    .child(detail.to_string()),
            )
            .into_any_element()
    }

    fn directory_empty_state(&self, cx: &App) -> AnyElement {
        let scanning = self.results.state == ScanState::Scanning;
        if scanning && self.results.is_empty() {
            return self.render_empty_state(
                "正在发现目录内容",
                "目录较多时可能需要一些时间；结果会持续出现。",
                IconName::Folder,
                true,
                cx,
            );
        }
        if self.results.state == ScanState::Failed && self.results.is_empty() {
            return self.render_empty_state(
                "扫描失败",
                "根目录无法读取。请查看扫描问题，或选择其他位置。",
                IconName::TriangleAlert,
                false,
                cx,
            );
        }
        let (title, detail) = if self.results.state == ScanState::Partial && self.results.is_empty()
        {
            ("扫描已停止", "尚未发现任何条目。重新扫描后会显示结果。")
        } else if self.directory < self.results.len() && !self.results.is_complete(self.directory) {
            if scanning {
                ("正在扫描此目录", "该目录尚未扫描完整，暂不视为空目录。")
            } else {
                (
                    "该目录尚未扫描完整",
                    "部分内容未被读取，因此不能视为空目录。",
                )
            }
        } else if self.results.state == ScanState::Idle {
            ("尚未开始扫描", "选择一个位置后开始分析。")
        } else {
            ("此目录没有条目", "这里没有文件或子目录。")
        };
        self.render_empty_state(title, detail, IconName::Folder, scanning, cx)
    }

    fn render_directory_body(&self, wide: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let content = if self.visible_len() == 0 {
            self.directory_empty_state(cx)
        } else {
            let scroll = self
                .views
                .get(&self.directory)
                .map(|view| view.scroll.clone())
                .unwrap_or_default();
            div()
                .id("directory-list")
                .flex_1()
                .min_h_0()
                .when_some(self.focus_handle.clone(), |this, handle| {
                    this.track_focus(&handle).on_key_down(cx.listener(
                        |this, event: &KeyDownEvent, _, cx| {
                            this.handle_key(event, cx);
                        },
                    ))
                })
                .child(
                    uniform_list(
                        ("entries", self.directory),
                        self.visible_len(),
                        cx.processor(move |this, range: Range<usize>, _, cx| {
                            range
                                .map(|index| this.render_row(index, wide, cx))
                                .collect()
                        }),
                    )
                    .size_full()
                    .track_scroll(&scroll),
                )
                .into_any_element()
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .bg(theme.background)
            .child(self.render_content_toolbar(cx))
            .child(self.render_table_header(wide, true, cx))
            .child(content)
            .into_any_element()
    }

    fn category_totals(&self) -> [u64; 10] {
        let mut totals = [0u64; 10];
        for id in self.results.child_ids(0) {
            if self.results.hard_link_to(id).is_some() {
                continue;
            }
            let bytes = self.results.bytes(id);
            if bytes == 0 {
                continue;
            }
            let name = self.results.name(id).to_string_lossy().into_owned();
            let category = Category::classify(&name, self.results.kind(id));
            totals[category.index()] = totals[category.index()].saturating_add(bytes);
        }
        totals
    }

    fn render_composition(&self, cx: &App) -> Option<AnyElement> {
        if self.results.root_overflow() {
            return None;
        }
        let palette = Palette::for_theme(cx.theme());
        let totals = self.category_totals();
        let total = totals
            .iter()
            .fold(0u64, |sum, bytes| sum.saturating_add(*bytes));
        if total == 0 {
            return None;
        }
        let mut rows: Vec<(Category, u64)> = Category::ALL
            .into_iter()
            .zip(totals)
            .filter(|(_, bytes)| *bytes > 0)
            .collect();
        rows.sort_by(|left, right| {
            right
                .1
                .cmp(&left.1)
                .then_with(|| left.0.index().cmp(&right.0.index()))
        });

        let mut bar = h_flex()
            .h(px(6.0))
            .w_full()
            .rounded_full()
            .overflow_hidden()
            .bg(palette.bar_track);
        for (category, bytes) in &rows {
            bar = bar.child(
                div()
                    .h_full()
                    .w(relative(*bytes as f32 / total as f32))
                    .bg(palette.categories.color(*category)),
            );
        }

        let mut legend_rows = rows.clone();
        if legend_rows.len() > 6 {
            let rest = legend_rows.split_off(5);
            let rest_bytes = rest
                .iter()
                .fold(0u64, |sum, (_, bytes)| sum.saturating_add(*bytes));
            match legend_rows
                .iter_mut()
                .find(|(category, _)| *category == Category::Other)
            {
                Some((_, bytes)) => *bytes = bytes.saturating_add(rest_bytes),
                None => legend_rows.push((Category::Other, rest_bytes)),
            }
            legend_rows.sort_by(|left, right| {
                right
                    .1
                    .cmp(&left.1)
                    .then_with(|| left.0.index().cmp(&right.0.index()))
            });
        }

        let mut legend = h_flex().gap_3().flex_wrap();
        for (category, bytes) in &legend_rows {
            let percent = *bytes as f64 / total as f64 * 100.0;
            let percent = if percent < 0.5 {
                "<1%".to_string()
            } else {
                format!("{percent:.0}%")
            };
            legend = legend.child(
                h_flex()
                    .flex_shrink_0()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .size(px(6.0))
                            .rounded_full()
                            .bg(palette.categories.color(*category)),
                    )
                    .child(
                        div()
                            .text_size(px(type_size::CAPTION))
                            .text_color(palette.text_tertiary)
                            .child(category.label()),
                    )
                    .child(
                        div()
                            .text_size(px(type_size::CAPTION))
                            .text_color(palette.text_secondary)
                            .font_features(tabular_numbers())
                            .child(percent),
                    ),
            );
        }

        Some(
            v_flex()
                .px_3()
                .py_2()
                .gap_2()
                .border_b_1()
                .border_color(palette.border)
                .child(bar)
                .child(legend)
                .into_any_element(),
        )
    }

    fn render_overview_body(
        &self,
        wide: bool,
        show_largest: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let palette = Palette::for_theme(&theme);
        let count = self.visible_len();
        let content = if count == 0 {
            self.directory_empty_state(cx)
        } else {
            let scroll = self.overview_view.scroll.clone();
            div()
                .id("overview-list")
                .flex_1()
                .min_h_0()
                .when_some(self.focus_handle.clone(), |this, handle| {
                    this.track_focus(&handle).on_key_down(cx.listener(
                        |this, event: &KeyDownEvent, _, cx| {
                            this.handle_key(event, cx);
                        },
                    ))
                })
                .child(
                    uniform_list(
                        ("overview", 0usize),
                        count,
                        cx.processor(move |this, range: Range<usize>, _, cx| {
                            range
                                .map(|index| this.render_row(index, wide, cx))
                                .collect()
                        }),
                    )
                    .size_full()
                    .track_scroll(&scroll),
                )
                .into_any_element()
        };

        let largest_len = self
            .large_files
            .visible_len(&self.results, LARGE_FILE_FLOOR)
            .min(6);
        let largest = (show_largest && largest_len > 0).then(|| {
            v_flex()
                .flex_shrink_0()
                .border_t_1()
                .border_color(palette.border)
                .child(
                    h_flex()
                        .h(px(32.0))
                        .px_3()
                        .gap_2()
                        .child(
                            div()
                                .text_size(px(type_size::LABEL))
                                .font_medium()
                                .child("最大的文件"),
                        )
                        .child(
                            div()
                                .text_size(px(type_size::CAPTION))
                                .text_color(palette.text_tertiary)
                                .child("≥ 100 MiB · 已去重的普通文件"),
                        )
                        .child(div().flex_1())
                        .child(
                            button("all-large-files", false, &theme)
                                .with_size(Size::Small)
                                .label("查看全部")
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.show_page(Page::LargeFiles, cx)
                                })),
                        ),
                )
                .children((0..largest_len).map(|index| {
                    let id = self.large_files.visible_id(LARGE_FILE_FLOOR, index);
                    self.render_compact_file_row(id, cx)
                }))
        });

        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .bg(theme.background)
            .child(
                h_flex()
                    .h(px(layout::CONTENT_TOOLBAR))
                    .bg(palette.toolbar)
                    .px_3()
                    .gap_3()
                    .child(
                        div()
                            .text_size(px(type_size::LABEL))
                            .font_medium()
                            .child("空间去向"),
                    )
                    .child(
                        div()
                            .text_size(px(type_size::CAPTION))
                            .text_color(palette.text_tertiary)
                            .child(format!(
                                "{} 个顶层条目 · 按已分配空间排序",
                                format_count(count)
                            )),
                    ),
            )
            .children(self.render_composition(cx))
            .child(self.render_table_header(wide, true, cx))
            .child(content)
            .children(largest)
            .into_any_element()
    }

    fn render_compact_file_row(&self, id: NodeId, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let palette = Palette::for_theme(&theme);
        let name = self.results.name(id).to_string_lossy().into_owned();
        let parent = self
            .results
            .parent(id)
            .map(|parent| self.results.path(parent).display().to_string())
            .unwrap_or_default();
        let selected = self.overview_view.selected == Some(id);
        div()
            .id(ElementId::named_usize("overview-file", id))
            .relative()
            .w_full()
            .h(px(layout::ROW_COMPACT))
            .px_3()
            .gap_3()
            .flex()
            .items_center()
            .border_b_1()
            .border_color(palette.border)
            .bg(if selected {
                theme.list_active
            } else {
                theme.transparent
            })
            .when(!selected, |this| {
                this.hover(|style| style.bg(theme.list_hover))
            })
            .cursor_pointer()
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.focus_list(window, cx);
                this.overview_view.selected = Some(id);
                if event.click_count() == 2 {
                    this.open_parent();
                }
                cx.notify();
            }))
            .children(selected.then(|| {
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(2.0))
                    .bg(palette.selected_border)
            }))
            .child(
                Icon::new(IconName::File)
                    .with_size(px(13.0))
                    .text_color(if selected {
                        palette.data_accent
                    } else {
                        palette.text_tertiary
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(type_size::LABEL))
                    .text_color(if selected {
                        palette.on_selected
                    } else {
                        palette.text_primary
                    })
                    .child(name),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(type_size::CAPTION))
                    .text_color(palette.text_tertiary)
                    .child(parent),
            )
            .child(
                div()
                    .w(px(56.0))
                    .text_right()
                    .text_size(px(type_size::CAPTION))
                    .text_color(palette.text_secondary)
                    .font_features(tabular_numbers())
                    .child(self.share_label(id)),
            )
            .child(
                div()
                    .w(px(88.0))
                    .text_right()
                    .text_size(px(type_size::LABEL))
                    .font_medium()
                    .font_features(tabular_numbers())
                    .text_color(if selected {
                        palette.on_selected
                    } else {
                        palette.text_primary
                    })
                    .child(self.entry_size(id)),
            )
            .into_any_element()
    }

    fn render_large_files_body(&self, wide: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let palette = Palette::for_theme(&theme);
        let content = if self.visible_len() == 0 {
            let scanning = self.results.state == ScanState::Scanning;
            self.render_empty_state(
                if scanning {
                    "尚未发现符合条件的大文件"
                } else {
                    "当前阈值下没有文件"
                },
                if scanning {
                    "扫描仍在继续；可以降低阈值或等待更多结果。"
                } else {
                    "降低最小已分配空间阈值，或选择其他位置重新扫描。"
                },
                IconName::Files,
                scanning,
                cx,
            )
        } else {
            let scroll = self.large_file_view.scroll.clone();
            div()
                .id("large-file-list")
                .flex_1()
                .min_h_0()
                .when_some(self.focus_handle.clone(), |this, handle| {
                    this.track_focus(&handle).on_key_down(cx.listener(
                        |this, event: &KeyDownEvent, _, cx| {
                            this.handle_key(event, cx);
                        },
                    ))
                })
                .child(
                    uniform_list(
                        ("large-files", 0usize),
                        self.visible_len(),
                        cx.processor(move |this, range: Range<usize>, _, cx| {
                            range
                                .map(|index| this.render_row(index, wide, cx))
                                .collect()
                        }),
                    )
                    .size_full()
                    .track_scroll(&scroll),
                )
                .into_any_element()
        };

        let thresholds = [
            (LARGE_FILE_FLOOR, "≥100 MiB"),
            (1024 * 1024 * 1024, "≥1 GiB"),
            (10 * 1024 * 1024 * 1024, "≥10 GiB"),
            (0, "全部文件"),
        ];
        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .bg(theme.background)
            .child(
                h_flex()
                    .h(px(layout::CONTENT_TOOLBAR))
                    .bg(palette.toolbar)
                    .px_3()
                    .gap_2()
                    .child(
                        v_flex()
                            .gap_0()
                            .child(
                                div()
                                    .text_size(px(type_size::LABEL))
                                    .font_medium()
                                    .child("大文件"),
                            )
                            .child(
                                div()
                                    .text_size(px(type_size::CAPTION))
                                    .text_color(palette.text_tertiary)
                                    .child("整个扫描范围 · 普通文件 · 已去重"),
                            ),
                    )
                    .child(div().flex_1())
                    .children(thresholds.into_iter().map(|(minimum, label)| {
                        let selected = self.minimum_bytes == minimum;
                        button(("threshold", minimum as usize), false, &theme)
                            .with_size(Size::Small)
                            .label(label)
                            .when(selected, |button| button.secondary())
                            .when(!selected, |button| button.ghost())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_minimum(minimum);
                                cx.notify();
                            }))
                    })),
            )
            .child(self.render_table_header(wide, false, cx))
            .child(content)
            .into_any_element()
    }

    fn render_issues_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let palette = Palette::for_theme(&theme);
        let issue_border = palette.border;
        let issue_icon = theme.yellow;
        let issue_muted = palette.text_tertiary;
        let content = if self.results.issues.is_empty() {
            let scanning = self.results.state == ScanState::Scanning;
            self.render_empty_state(
                if scanning {
                    "正在扫描，暂无读取失败"
                } else {
                    "没有读取失败"
                },
                if scanning {
                    "无法读取的路径会在扫描过程中出现在这里。"
                } else {
                    "本次扫描没有遇到无法读取的目录或文件。"
                },
                IconName::ShieldCheck,
                scanning,
                cx,
            )
        } else {
            div()
                .flex_1()
                .min_h_0()
                .child(
                    uniform_list(
                        ("issues", 0usize),
                        self.results.issues.len(),
                        cx.processor(move |this, range: Range<usize>, _, _| {
                            range
                                .map(|index| {
                                    let issue = &this.results.issues[index];
                                    div()
                                        .w_full()
                                        .min_h(px(48.0))
                                        .px_3()
                                        .py_2()
                                        .gap_3()
                                        .flex()
                                        .items_start()
                                        .border_b_1()
                                        .border_color(issue_border)
                                        .child(
                                            Icon::new(IconName::TriangleAlert)
                                                .with_size(px(13.0))
                                                .text_color(issue_icon),
                                        )
                                        .child(
                                            v_flex()
                                                .flex_1()
                                                .min_w_0()
                                                .gap_1()
                                                .child(
                                                    div()
                                                        .text_size(px(type_size::LABEL))
                                                        .truncate()
                                                        .child(issue.path.display().to_string()),
                                                )
                                                .child(
                                                    div()
                                                        .text_size(px(type_size::CAPTION))
                                                        .text_color(issue_muted)
                                                        .line_clamp(2)
                                                        .child(issue.message.clone()),
                                                ),
                                        )
                                        .into_any_element()
                                })
                                .collect()
                        }),
                    )
                    .size_full()
                    .track_scroll(&self.issue_scroll),
                )
                .into_any_element()
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .bg(theme.background)
            .child(
                h_flex()
                    .h(px(layout::CONTENT_TOOLBAR))
                    .bg(palette.toolbar)
                    .px_3()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(type_size::LABEL))
                            .font_medium()
                            .child("读取失败与扫描问题"),
                    )
                    .child(
                        div()
                            .text_size(px(type_size::CAPTION))
                            .text_color(palette.text_tertiary)
                            .child(format_count(self.results.issues.len())),
                    ),
            )
            .child(content)
            .into_any_element()
    }

    fn render_inspector(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let palette = Palette::for_theme(&theme);
        let selected = self.selected();
        let body = match selected {
            None => v_flex()
                .items_center()
                .justify_center()
                .gap_2()
                .p_6()
                .child(
                    Icon::new(IconName::MousePointer2)
                        .with_size(px(20.0))
                        .text_color(palette.text_tertiary),
                )
                .child(
                    div()
                        .text_size(px(type_size::BODY))
                        .font_medium()
                        .child("选择一个条目查看详情"),
                )
                .child(
                    div()
                        .max_w(px(220.0))
                        .text_center()
                        .text_size(px(type_size::CAPTION))
                        .text_color(palette.text_tertiary)
                        .child("显示完整路径、大小占比、扫描状态与归属说明。"),
                )
                .child(
                    div()
                        .max_w(px(220.0))
                        .text_center()
                        .text_size(px(type_size::CAPTION))
                        .text_color(palette.text_tertiary)
                        .child("Clean 只读取文件系统信息，不修改任何文件。"),
                )
                .into_any_element(),
            Some(id) => {
                let name = self.results.name(id).to_string_lossy().into_owned();
                let path = self.results.to_path_buf(id);
                let kind = self.results.kind(id);
                let is_directory = kind == EntryKind::Directory;
                let size = self.entry_size(id);
                let share = self.share_label(id);
                let category = Category::classify(&name, kind);
                let category_color = palette.categories.color(category);
                let icon = match kind {
                    EntryKind::Directory => IconName::Folder,
                    EntryKind::File => IconName::File,
                    EntryKind::Symlink => IconName::Link2,
                    EntryKind::Other => IconName::FileQuestionMark,
                };
                let mut details = Vec::new();
                if self.page == Page::Directory
                    && self.directory != 0
                    && self.results.hard_link_to(id).is_none()
                {
                    details.push(("占扫描总量", self.root_share_label(id)));
                }
                if is_directory {
                    details.push(("直接子项", format_count(self.results.child_count(id))));
                    details.push(("扫描状态", self.directory_state_label(id)));
                }
                if self.results.errors(id) > 0 {
                    details.push((
                        "读取失败",
                        format!("{} 项", format_count(self.results.errors(id))),
                    ));
                }
                if let Some(owner) = self.results.hard_link_to(id) {
                    details.push(("已计入", self.results.path(owner).display().to_string()));
                }

                let reveal_label = if self.revealing {
                    "正在定位…".to_string()
                } else {
                    platform::reveal_label()
                };
                let primary = match (is_directory, self.page) {
                    (true, _) => Some(
                        button("enter-directory", false, &theme)
                            .primary()
                            .with_size(Size::Small)
                            .w_full()
                            .label("进入目录")
                            .icon(IconName::CornerDownRight)
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.navigate(id, true, cx)),
                            )
                            .into_any_element(),
                    ),
                    (false, Page::LargeFiles) => Some(
                        button("go-parent", false, &theme)
                            .primary()
                            .with_size(Size::Small)
                            .w_full()
                            .label("前往所在目录")
                            .icon(IconName::CornerDownRight)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.open_parent();
                                cx.notify();
                            }))
                            .into_any_element(),
                    ),
                    (false, _) => Some(
                        button("inspector-reveal-primary", self.revealing, &theme)
                            .primary()
                            .with_size(Size::Small)
                            .w_full()
                            .label(reveal_label)
                            .icon(IconName::FolderSearch)
                            .on_click(cx.listener(|this, _, _, cx| this.reveal_selected(cx)))
                            .into_any_element(),
                    ),
                };
                let show_secondary_reveal = is_directory || self.page == Page::LargeFiles;

                v_flex()
                    .gap_4()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_start()
                            .child(Icon::new(icon).with_size(px(18.0)).text_color(
                                if is_directory {
                                    palette.folder
                                } else {
                                    palette.text_tertiary
                                },
                            ))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_size(px(type_size::HEADING))
                                            .font_medium()
                                            .truncate()
                                            .child(name),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .items_center()
                                            .child(
                                                div()
                                                    .size(px(6.0))
                                                    .rounded_full()
                                                    .bg(category_color),
                                            )
                                            .child(
                                                div()
                                                    .text_size(px(type_size::CAPTION))
                                                    .text_color(palette.text_tertiary)
                                                    .child(format!(
                                                        "{} · {}",
                                                        self.kind_label(id),
                                                        category.label()
                                                    )),
                                            ),
                                    ),
                            ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .pt_1()
                            .child(
                                div()
                                    .text_size(px(type_size::DISPLAY))
                                    .font_semibold()
                                    .font_features(tabular_numbers())
                                    .child(size),
                            )
                            .child(
                                div()
                                    .text_size(px(type_size::CAPTION))
                                    .text_color(palette.text_tertiary)
                                    .child(format!("{} {}", self.share_caption(), share)),
                            ),
                    )
                    .child(div().h(px(1.0)).w_full().bg(palette.border))
                    .child(
                        v_flex().gap_2().children(
                            details
                                .into_iter()
                                .map(|(label, value)| detail_row(label, value, &theme)),
                        ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(type_size::CAPTION))
                                    .text_color(palette.text_tertiary)
                                    .child("完整路径"),
                            )
                            .child(
                                div()
                                    .text_size(px(type_size::CAPTION))
                                    .font_family(theme.mono_font_family.clone())
                                    .text_color(palette.text_secondary)
                                    .whitespace_normal()
                                    .line_clamp(3)
                                    .child(path.display().to_string()),
                            ),
                    )
                    .child(
                        div()
                            .pt_2()
                            .border_t_1()
                            .border_color(palette.border)
                            .text_size(px(type_size::CAPTION))
                            .text_color(palette.text_tertiary)
                            .line_height(px(17.0))
                            .whitespace_normal()
                            .child(self.explanation(id)),
                    )
                    .child(div().h(px(1.0)).w_full().bg(palette.border))
                    .child(
                        v_flex()
                            .gap_1()
                            .children(primary)
                            .children(show_secondary_reveal.then(|| {
                                self.render_action_row(
                                    "inspector-reveal",
                                    if self.revealing {
                                        "正在定位…".into()
                                    } else {
                                        platform::reveal_label()
                                    },
                                    IconName::FolderSearch,
                                    palette.text_secondary,
                                    !self.revealing,
                                    |this, _, _, cx| this.reveal_selected(cx),
                                    cx,
                                )
                            }))
                            .child(self.render_action_row(
                                "inspector-copy",
                                "复制路径".into(),
                                IconName::Copy,
                                palette.text_tertiary,
                                true,
                                |this, _, _, cx| this.copy_selected(cx),
                                cx,
                            )),
                    )
                    .into_any_element()
            }
        };

        v_flex()
            .w(px(layout::INSPECTOR))
            .h_full()
            .flex_shrink_0()
            .bg(palette.toolbar)
            .border_l_1()
            .border_color(palette.border)
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .px_4()
                    .py_4()
                    .child(body),
            )
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_action_row<F>(
        &self,
        id: &'static str,
        label: String,
        icon: IconName,
        color: Hsla,
        enabled: bool,
        handler: F,
        cx: &mut Context<Self>,
    ) -> AnyElement
    where
        F: Fn(&mut Self, &ClickEvent, &mut Window, &mut Context<Self>) + 'static,
    {
        let palette = Palette::for_theme(cx.theme());
        h_flex()
            .id(id)
            .w_full()
            .h(px(layout::ROW_COMPACT))
            .px_2()
            .rounded(px(radius::CONTROL))
            .gap_2()
            .items_center()
            .text_size(px(type_size::LABEL))
            .text_color(if enabled {
                color
            } else {
                palette.text_disabled
            })
            .when(enabled, |this| {
                this.cursor_pointer()
                    .hover(|style| style.bg(palette.hover))
                    .on_click(cx.listener(handler))
            })
            .child(Icon::new(icon).with_size(px(14.0)).text_color(if enabled {
                color
            } else {
                palette.text_disabled
            }))
            .child(label)
            .into_any_element()
    }

    fn root_share_label(&self, id: NodeId) -> String {
        if self.results.is_empty() || self.results.root_overflow() || self.page == Page::Issues {
            return "—".into();
        }
        if self.results.hard_link_to(id).is_some() {
            return "已去重".into();
        }
        let total = self.results.root_bytes();
        if total == 0 {
            return "—".into();
        }
        let ratio = (self.results.bytes(id) as f64 / total as f64) as f32;
        if ratio > 0.0 && ratio < 0.001 {
            "<0.1%".into()
        } else {
            format!("{:.1}%", ratio * 100.0)
        }
    }

    fn directory_state_label(&self, id: NodeId) -> String {
        if self.results.size_overflow(id) {
            return "大小超出统计范围".into();
        }
        if self.results.errors(id) > 0 {
            return format!("{} 项读取失败", format_count(self.results.errors(id)));
        }
        if !self.results.is_complete(id) {
            return if self.results.state == ScanState::Scanning {
                "正在扫描".into()
            } else {
                "尚未扫描完整".into()
            };
        }
        "完整".into()
    }

    fn explanation(&self, id: NodeId) -> String {
        if let Some(owner) = self.results.hard_link_to(id) {
            return format!(
                "此路径是同一文件的硬链接别名。空间已计入首次发现的路径 {}，不重复统计。",
                self.results.path(owner).display()
            );
        }
        match self.results.kind(id) {
            EntryKind::Symlink => "符号链接不计入空间总量，扫描不会跟随链接目标。".into(),
            EntryKind::Other => "特殊文件（设备、管道等）不计入空间总量。".into(),
            EntryKind::Directory if self.results.size_overflow(id) => {
                "该目录的已分配空间超出统计范围，大小与占比不精确。".into()
            }
            EntryKind::Directory if self.results.errors(id) > 0 => {
                "部分内容无法读取，其空间尚未计入。这不表示这些路径是空的。".into()
            }
            EntryKind::Directory if !self.results.is_complete(id) => {
                if self.results.state == ScanState::Scanning {
                    "该目录仍在扫描，当前大小会继续增加。".into()
                } else {
                    "该目录未扫描完整，当前大小不是最终值。".into()
                }
            }
            EntryKind::Directory if self.results.child_count(id) == 0 => {
                "这是一个空目录。目录自身仍可能有少量分配空间。".into()
            }
            _ => "大小为文件系统报告的已分配空间；它不是独占占用，也不等于删除后可释放的空间。"
                .into(),
        }
    }

    fn render_onboarding(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let palette = Palette::for_theme(&theme);
        let home = platform::home_directory();
        let downloads = platform::downloads_directory();
        let home_enabled = home.is_some() && !self.choosing;
        let downloads_enabled = downloads.is_some() && !self.choosing;
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_4()
            .p_8()
            .child(
                div()
                    .size(px(48.0))
                    .rounded(px(radius::LARGE))
                    .bg(theme.selection)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::new(IconName::HardDrive)
                            .with_size(px(24.0))
                            .text_color(theme.chart_2),
                    ),
            )
            .child(
                v_flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .text_size(px(type_size::TITLE))
                            .font_semibold()
                            .child("分析磁盘空间去了哪里"),
                    )
                    .child(
                        div()
                            .max_w(px(460.0))
                            .text_center()
                            .text_size(px(type_size::BODY))
                            .text_color(theme.muted_foreground)
                            .child(
                                "选择一个位置，Clean 会按文件系统报告的已分配空间统计目录与文件。扫描期间即可浏览，随时可以停止。",
                            ),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .when_some(home, |this, home| {
                        this.child(
                            button("onboard-home", !home_enabled, &theme)
                                .label("扫描主目录")
                                .icon(IconName::House)
                                .primary()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.start_scan(home.clone(), cx)
                                })),
                        )
                    })
                    .when_some(downloads, |this, downloads| {
                        this.child(
                            button("onboard-downloads", !downloads_enabled, &theme)
                                .label("下载")
                                .icon(IconName::Download)
                                .secondary()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.start_scan(downloads.clone(), cx)
                                })),
                        )
                    })
                    .child(
                        button("onboard-choose", self.choosing, &theme)
                            .label("选择目录…")
                            .icon(IconName::FolderOpen)
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.choose_directory(cx)
                            })),
                    ),
            )
            .child(
                div()
                    .text_size(px(type_size::CAPTION))
                    .text_color(palette.text_tertiary)
                    .child("只读分析 · 不删除或修改任何文件"),
            )
            .into_any_element()
    }

    fn render_status_bar(&self, compact: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let palette = Palette::for_theme(&theme);
        let selected = self.selected();
        let directory_selected = selected
            .filter(|id| self.results.kind(*id) == EntryKind::Directory)
            .is_some();
        let left = if let Some(id) = selected {
            let path = self.results.to_path_buf(id);
            h_flex()
                .gap_2()
                .min_w_0()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(type_size::CAPTION))
                        .text_color(palette.text_tertiary)
                        .child(path.display().to_string()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(type_size::CAPTION))
                        .text_color(palette.text_tertiary)
                        .font_features(tabular_numbers())
                        .child(self.entry_size(id)),
                )
                .into_any_element()
        } else {
            div()
                .text_size(px(type_size::CAPTION))
                .text_color(palette.text_tertiary)
                .child("已分配空间不等于可释放空间")
                .into_any_element()
        };

        let copy = platform::COPY_SHORTCUT;
        let inspector = platform::INSPECTOR_SHORTCUT;
        let back = platform::BACK_SHORTCUT;
        let hints = match self.page {
            Page::Directory if directory_selected => {
                format!("Enter 进入 · {back} 返回 · {copy} 复制")
            }
            Page::Directory if selected.is_none() => "↑↓ 选择 · Enter 进入".into(),
            Page::Directory => format!("{back} 返回 · {copy} 复制 · {inspector} 详情"),
            Page::Overview if directory_selected => {
                format!("Enter 进入 · {copy} 复制 · {inspector} 详情")
            }
            Page::Overview if selected.is_none() => "↑↓ 选择 · Enter 进入".into(),
            Page::Overview => format!("{copy} 复制 · {inspector} 详情"),
            Page::LargeFiles if selected.is_none() => "↑↓ 选择 · Enter 所在目录".into(),
            Page::LargeFiles => format!("Enter 所在目录 · {copy} 复制路径"),
            Page::Issues => String::new(),
        };

        h_flex()
            .h(px(layout::STATUS))
            .px_3()
            .gap_3()
            .bg(theme.status_bar)
            .border_t_1()
            .border_color(palette.border)
            .child(left)
            .child(div().flex_1())
            .when(!compact && !hints.is_empty(), |this| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(type_size::CAPTION))
                        .text_color(palette.text_disabled)
                        .child(hints),
                )
            })
            .into_any_element()
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().background;
        let foreground = cx.theme().foreground;
        let width = window.viewport_size().width.as_f32();
        let height = window.viewport_size().height.as_f32();
        let compact = width < 980.0;
        let wide_rows = width
            >= if self.page == Page::LargeFiles {
                1160.0
            } else {
                1000.0
            };
        let show_inspector = self.show_inspector && width >= 1080.0 && self.page != Page::Issues;
        let has_scan = self.scan_root.is_some();
        let idle = self.results.state == ScanState::Idle && !has_scan && self.results.is_empty();

        let sidebar = self.render_sidebar(cx);
        let topbar = self.render_topbar(compact, cx);
        let status = self.render_status_bar(compact, cx);
        let summary = (!idle).then(|| self.render_summary(compact, cx));
        let body = if idle {
            self.render_onboarding(cx)
        } else {
            let content = match self.page {
                Page::Overview => self.render_overview_body(wide_rows, height >= 620.0, cx),
                Page::Directory => self.render_directory_body(wide_rows, cx),
                Page::LargeFiles => self.render_large_files_body(wide_rows, cx),
                Page::Issues => self.render_issues_body(cx),
            };
            let mut body = div()
                .flex()
                .flex_row()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(content);
            if show_inspector {
                body = body.child(self.render_inspector(cx));
            }
            body.into_any_element()
        };

        let mut main = v_flex().flex_1().min_w_0().h_full();
        main = main.child(topbar);
        if let Some(summary) = summary {
            main = main.child(summary);
        }
        main = main.child(body).child(status);

        div()
            .size_full()
            .flex()
            .bg(background)
            .text_color(foreground)
            .font(Font {
                fallbacks: Some(FontFallbacks::from_fonts(vec!["PingFang SC".into()])),
                ..font(cx.theme().font_family.clone())
            })
            .font_weight(FontWeight::NORMAL)
            .text_size(px(type_size::BODY))
            .child(sidebar)
            .child(main)
    }
}

fn section_label(text: &str, color: Hsla) -> AnyElement {
    div()
        .h(px(22.0))
        .px_2()
        .flex()
        .items_center()
        .text_size(px(type_size::CAPTION))
        .text_color(color)
        .child(text.to_string())
        .into_any_element()
}

fn metric(label: &str, value: String, theme: &Theme) -> AnyElement {
    let palette = Palette::for_theme(theme);
    v_flex()
        .min_w(px(52.0))
        .gap(px(2.0))
        .child(
            div()
                .text_size(px(type_size::CAPTION))
                .text_color(palette.text_tertiary)
                .child(label.to_string()),
        )
        .child(
            div()
                .text_size(px(type_size::BODY))
                .font_medium()
                .font_features(tabular_numbers())
                .child(value),
        )
        .into_any_element()
}

fn detail_row(label: &str, value: String, theme: &Theme) -> AnyElement {
    h_flex()
        .justify_between()
        .items_start()
        .gap_3()
        .text_size(px(type_size::CAPTION))
        .child(
            div()
                .flex_shrink_0()
                .text_color(Palette::for_theme(theme).text_tertiary)
                .child(label.to_string()),
        )
        .child(
            div()
                .min_w_0()
                .text_right()
                .text_color(theme.muted_foreground)
                .whitespace_normal()
                .child(value),
        )
        .into_any_element()
}

fn tabular_numbers() -> FontFeatures {
    FontFeatures(Arc::new(vec![("tnum".to_string(), 1)]))
}

fn format_count(value: usize) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(ch);
    }
    output
}

fn duration_text(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();
    if seconds < 60.0 {
        return format!("{seconds:.1} 秒");
    }
    let minutes = duration.as_secs() / 60;
    let rest = duration.as_secs() % 60;
    if minutes < 60 {
        format!("{minutes} 分 {rest:02} 秒")
    } else {
        let hours = minutes / 60;
        format!("{hours} 小时 {:02} 分", minutes % 60)
    }
}

#[cfg(test)]
mod tests;
