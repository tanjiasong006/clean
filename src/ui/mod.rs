use std::{
    collections::HashMap,
    ops::Range,
    path::PathBuf,
    sync::mpsc::TryRecvError,
    time::{Duration, Instant},
};

use gpui_kit::component::{ActiveTheme, Disableable, button::Button};
use gpui_kit::*;

use crate::{
    analysis::{DirectoryIndex, LargeFileIndex, format_bytes},
    models::{DiskSpace, EntryKind, NodeId, ScanEvent, ScanIssue, ScanResults, ScanState},
    platform,
    scanner::ScanJob,
};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Page {
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

pub struct AppView {
    results: ScanResults,
    job: Option<ScanJob>,
    poll_task: Option<Task<()>>,
    directory: NodeId,
    visible: Vec<NodeId>,
    views: HashMap<NodeId, DirectoryView>,
    history: Vec<NodeId>,
    page: Page,
    large_files: LargeFileIndex,
    directory_index: DirectoryIndex,
    last_refresh: Instant,
    visible_dirty: bool,
    large_file_view: DirectoryView,
    minimum_bytes: u64,
    disk_space: Option<Result<DiskSpace, String>>,
    disk_task: Option<Task<()>>,
    revealing: bool,
    issue_scroll: UniformListScrollHandle,
    cancelling: bool,
    choosing: bool,
    notice: Option<String>,
    scan_root: Option<PathBuf>,
}

impl Default for AppView {
    fn default() -> Self {
        Self {
            results: ScanResults::default(),
            job: None,
            poll_task: None,
            directory: 0,
            visible: Vec::new(),
            views: HashMap::new(),
            history: Vec::new(),
            page: Page::Directory,
            large_files: LargeFileIndex::default(),
            directory_index: DirectoryIndex::default(),
            last_refresh: Instant::now(),
            visible_dirty: false,
            large_file_view: DirectoryView::default(),
            minimum_bytes: 100 * 1024 * 1024,
            disk_space: None,
            disk_task: None,
            revealing: false,
            issue_scroll: UniformListScrollHandle::new(),
            cancelling: false,
            choosing: false,
            notice: None,
            scan_root: None,
        }
    }
}

impl AppView {
    pub fn new(_: &mut Context<Self>) -> Self {
        Self::default()
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
        self.results = ScanResults {
            state: ScanState::Scanning,
            ..Default::default()
        };
        self.job = Some(job);
        self.scan_root = Some(path.clone());
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
        self.visible.clear();
        self.visible_dirty = false;
        self.views.clear();
        self.views.insert(0, DirectoryView::default());
        self.history.clear();
        self.large_files = LargeFileIndex::default();
        self.directory_index = DirectoryIndex::default();
        self.large_file_view = DirectoryView::default();
        self.page = Page::Directory;
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
        let root = self.results.nodes.first()?;
        let space = self.disk_space.as_ref()?.as_ref().ok()?;
        (!root.size_overflow && root.bytes > space.capacity).then_some(
            "逻辑总量已超过所选卷容量，但不代表实际磁盘占用。稀疏文件、硬链接或跨卷扫描等均可能造成差异。",
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
                        self.results.state = if self.results.nodes.is_empty() {
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
            self.job = None;
            self.cancelling = false;
            return false;
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
        self.visible = match self.page {
            Page::Directory => {
                self.directory_index.select(&self.results, self.directory);
                self.directory_index.sorted()
            }
            Page::LargeFiles => self.large_files.matching(self.minimum_bytes),
            Page::Issues => Vec::new(),
        };
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
        if self.page == Page::LargeFiles
            && let Some(id) = self.selected()
            && let Some(parent) = self.results.nodes[id].parent
        {
            self.navigate_to(parent, true);
            self.views.entry(parent).or_default().selected = Some(id);
            if let Some(index) = self.visible.iter().position(|item| *item == id) {
                self.views
                    .entry(parent)
                    .or_default()
                    .scroll
                    .scroll_to_item(index, ScrollStrategy::Center);
            }
        }
    }

    fn selected(&self) -> Option<NodeId> {
        match self.page {
            Page::Directory => self
                .views
                .get(&self.directory)
                .and_then(|view| view.selected),
            Page::LargeFiles => self.large_file_view.selected,
            Page::Issues => None,
        }
    }

    fn reveal_selected(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.selected() else { return };
        if self.revealing {
            return;
        }
        let path = self.results.nodes[id].path.clone();
        self.revealing = true;
        self.notice = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { platform::reveal_in_finder(&path) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.revealing = false;
                if let Err(error) = result {
                    this.notice = Some(format!("无法在 Finder 中定位：{error}"));
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let id = self.visible[index];
        let node = &self.results.nodes[id];
        let selected = self.selected() == Some(id);
        let large_file = self.page == Page::LargeFiles;
        let is_directory = node.kind == EntryKind::Directory;
        let prefix = match node.kind {
            EntryKind::Directory => "▸",
            EntryKind::File => "·",
            EntryKind::Symlink => "↗",
            EntryKind::Other => "–",
        };
        let name = node
            .path
            .file_name()
            .unwrap_or(node.path.as_os_str())
            .to_string_lossy()
            .into_owned();
        let status = match node.kind {
            EntryKind::Symlink => "符号链接 · 已跳过",
            EntryKind::Other => "特殊文件 · 已跳过",
            EntryKind::Directory if node.size_overflow => "逻辑大小超出统计范围",
            EntryKind::Directory if node.errors > 0 => "含读取失败项",
            EntryKind::Directory if !node.is_complete() => "尚未扫描完整",
            EntryKind::Directory if node.children.is_empty() => "空目录",
            EntryKind::Directory => "目录 · 双击进入",
            EntryKind::File => "文件",
        };
        let description = if large_file {
            node.path.display().to_string()
        } else {
            status.to_string()
        };
        let denominator = &self.results.nodes[if large_file { 0 } else { self.directory }];
        let total = denominator.bytes;
        let ratio = if total == 0 {
            0.0
        } else {
            node.bytes as f32 / total as f32
        };
        div()
            .id(id)
            .w_full()
            .h(px(64.0))
            .px_4()
            .flex()
            .items_center()
            .gap_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(if selected {
                cx.theme().secondary
            } else {
                cx.theme().background
            })
            .hover(|style| style.bg(cx.theme().secondary))
            .cursor_pointer()
            .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                if large_file {
                    this.large_file_view.selected = Some(id);
                } else {
                    this.views.entry(this.directory).or_default().selected = Some(id);
                }
                if is_directory && event.click_count() == 2 {
                    this.navigate(id, true, cx);
                }
                cx.notify();
            }))
            .child(div().w_5().child(prefix))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().child(name))
                    .child(
                        div()
                            .text_xs()
                            .truncate()
                            .text_color(cx.theme().muted_foreground)
                            .child(description),
                    ),
            )
            .child(if denominator.size_overflow {
                div()
                    .w(px(110.0))
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("占比未知")
                    .into_any_element()
            } else {
                div()
                    .w(px(110.0))
                    .h(px(5.0))
                    .rounded_full()
                    .bg(cx.theme().secondary)
                    .child(
                        div()
                            .h_full()
                            .w(relative(ratio))
                            .rounded_full()
                            .bg(cx.theme().primary),
                    )
                    .into_any_element()
            })
            .child(
                div()
                    .w(px(100.0))
                    .text_right()
                    .child(if node.size_overflow {
                        "超出范围".into()
                    } else {
                        format_bytes(node.bytes)
                    }),
            )
            .into_any_element()
    }
}

impl Render for AppView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.job.is_some() || self.choosing;
        let home = platform::home_directory();
        let has_home = home.is_some();
        let downloads = home.as_ref().map(|home| home.join("Downloads"));
        let root_path = self
            .scan_root
            .as_ref()
            .map(|path| path.display().to_string());
        let logical_size = match self.results.nodes.first() {
            Some(node) if node.size_overflow => "逻辑大小超出统计范围".into(),
            Some(node) => format_bytes(node.bytes),
            None => format_bytes(0),
        };
        let current = self.results.nodes.get(self.directory);
        let mut ancestors = Vec::new();
        let mut ancestor = current.map(|_| self.directory);
        while let Some(id) = ancestor {
            ancestors.push(id);
            ancestor = self.results.nodes[id].parent;
        }
        ancestors.reverse();
        let selected = self.selected();
        let selected_path = selected.map(|id| self.results.nodes[id].path.clone());
        let content = if self.page == Page::Issues && self.results.issues.is_empty() {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child("当前没有扫描问题。")
                .into_any_element()
        } else if self.page == Page::Issues {
            div()
                .flex_1()
                .min_h_0()
                .child(
                    uniform_list(
                        "issues",
                        self.results.issues.len(),
                        cx.processor(|this, range: Range<usize>, _, cx| {
                            range
                                .map(|index| {
                                    let issue = &this.results.issues[index];
                                    div()
                                        .w_full()
                                        .h(px(76.0))
                                        .px_4()
                                        .py_2()
                                        .border_b_1()
                                        .border_color(cx.theme().border)
                                        .child(
                                            div()
                                                .truncate()
                                                .child(issue.path.display().to_string()),
                                        )
                                        .child(
                                            div()
                                                .text_sm()
                                                .truncate()
                                                .text_color(cx.theme().muted_foreground)
                                                .child(issue.message.clone()),
                                        )
                                })
                                .collect()
                        }),
                    )
                    .size_full()
                    .track_scroll(&self.issue_scroll),
                )
                .into_any_element()
        } else if self.visible.is_empty() {
            let message = match self.results.state {
                ScanState::Idle => "选择主目录、下载目录或其他目录开始分析。",
                ScanState::Scanning => "正在发现目录内容…",
                ScanState::Failed => "无法完成扫描，请查看扫描问题。",
                ScanState::Partial if self.results.nodes.is_empty() => {
                    "扫描已取消，尚未发现目录内容。"
                }
                _ if self.page == Page::LargeFiles => "已发现的文件中没有符合当前大小条件的条目。",
                _ if current.is_some_and(|node| !node.is_complete()) => {
                    "该目录未扫描完整，不能视为空目录。"
                }
                _ => "此目录为空。",
            };
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(message)
                .into_any_element()
        } else {
            let scroll = if self.page == Page::LargeFiles {
                self.large_file_view.scroll.clone()
            } else {
                self.views.entry(self.directory).or_default().scroll.clone()
            };
            div()
                .flex_1()
                .min_h_0()
                .child(
                    uniform_list(
                        (
                            "entries",
                            if self.page == Page::LargeFiles {
                                usize::MAX
                            } else {
                                self.directory
                            },
                        ),
                        self.visible.len(),
                        cx.processor(|this, range: Range<usize>, _, cx| {
                            range.map(|index| this.render_row(index, cx)).collect()
                        }),
                    )
                    .size_full()
                    .track_scroll(&scroll),
                )
                .into_any_element()
        };

        let sidebar = div()
            .w(px(210.0))
            .h_full()
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .border_r_1()
            .border_color(cx.theme().border)
            .child(div().text_2xl().child("Clean"))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("磁盘空间分析"),
            )
            .child(
                Button::new("home")
                    .label("扫描主目录")
                    .disabled(busy || !has_home)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(home) = home.clone() {
                            this.start_scan(home, cx);
                        }
                    })),
            )
            .child(
                Button::new("downloads")
                    .label("扫描下载目录")
                    .disabled(busy || !has_home)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(path) = downloads.clone() {
                            this.start_scan(path, cx);
                        }
                    })),
            )
            .child(
                Button::new("choose")
                    .label("选择其他目录…")
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| this.choose_directory(cx))),
            )
            .child(div().h_4())
            .child(
                Button::new("overview")
                    .label("目录分析")
                    .disabled(self.results.nodes.is_empty())
                    .on_click(cx.listener(|this, _, _, cx| this.show_page(Page::Directory, cx))),
            )
            .child(
                Button::new("large-files")
                    .label("大文件")
                    .disabled(self.results.nodes.is_empty())
                    .on_click(cx.listener(|this, _, _, cx| this.show_page(Page::LargeFiles, cx))),
            )
            .child(
                Button::new("issues")
                    .label(format!("扫描问题 ({})", self.results.issues.len()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_page(Page::Issues, cx);
                    })),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("仅分析，不删除任何文件。"),
            );

        let cancel = Button::new("cancel")
            .label(if self.cancelling {
                "正在取消…"
            } else {
                "取消扫描"
            })
            .disabled(self.job.is_none() || self.cancelling)
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(job) = &this.job {
                    job.cancel();
                    this.cancelling = true;
                    cx.notify();
                }
            }));
        let disk_label = match &self.disk_space {
            Some(Ok(space)) => format!(
                "所选卷容量 {} · 当前用户可用 {}（扫描开始时查询，与扫描总量独立）",
                format_bytes(space.capacity),
                format_bytes(space.available)
            ),
            Some(Err(error)) => format!("无法读取所选卷容量：{error}"),
            None if self.scan_root.is_some() => "正在查询所选卷容量…".into(),
            None => "选择目录后查询所选卷容量与可用空间。".into(),
        };
        let summary = div().p_5().flex().flex_col().gap_2()
            .border_b_1().border_color(cx.theme().border)
            .child(div().flex().justify_between().items_center()
                .child(div().text_lg().child(self.results.state.label())).child(cancel))
            .child(div().text_sm().truncate().text_color(cx.theme().muted_foreground)
                .child(root_path.unwrap_or_else(|| "尚未选择扫描位置".into())))
            .child(div().text_2xl().child(format!("逻辑总量 {} · {} 个文件 · {} 个目录",
                logical_size, self.results.files, self.results.directories)))
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(format!(
                "文件逻辑大小累加，非磁盘实际占用 · 硬链接按路径重复计数 · 跳过 {} 个符号链接 · 非可释放空间",
                self.results.symlinks)))
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(disk_label))
            .children(self.capacity_notice().map(|notice| div().text_sm().child(notice)))
            .children(self.notice.clone().map(|notice| div().text_sm().child(notice)));

        let crumbs = ancestors.into_iter().map(|id| {
            let path = &self.results.nodes[id].path;
            let name = if id == 0 {
                "扫描根目录".to_string()
            } else {
                path.file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy()
                    .into_owned()
            };
            Button::new(("breadcrumb", id))
                .label(name)
                .on_click(cx.listener(move |this, _, _, cx| this.navigate(id, true, cx)))
        });
        let breadcrumbs = div()
            .px_4()
            .py_2()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Button::new("back")
                    .label("返回")
                    .disabled(self.history.is_empty())
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(id) = this.history.pop() {
                            this.navigate(id, false, cx);
                        }
                    })),
            )
            .child(
                div()
                    .id("breadcrumbs")
                    .flex_1()
                    .min_w_0()
                    .overflow_x_scroll()
                    .child(div().flex().gap_2().children(crumbs)),
            );

        let toolbar = match self.page {
            Page::Directory => breadcrumbs.into_any_element(),
            Page::LargeFiles => div()
                .px_4()
                .py_2()
                .flex()
                .items_center()
                .gap_2()
                .child(div().text_sm().child("最小逻辑大小"))
                .children(
                    [
                        (0, "全部文件"),
                        (100 * 1024 * 1024, "100 MiB"),
                        (1024 * 1024 * 1024, "1 GiB"),
                        (10 * 1024 * 1024 * 1024, "10 GiB"),
                    ]
                    .into_iter()
                    .map(|(minimum, label)| {
                        Button::new(("minimum", minimum as usize))
                            .label(if self.minimum_bytes == minimum {
                                format!("✓ {label}")
                            } else {
                                label.into()
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.set_minimum(minimum);
                                cx.notify();
                            }))
                    }),
                )
                .into_any_element(),
            Page::Issues => div().into_any_element(),
        };
        let heading = div()
            .px_4()
            .py_2()
            .flex()
            .justify_between()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(if self.page == Page::Issues {
                "读取失败与扫描问题"
            } else if self.page == Page::LargeFiles {
                "大文件 · 整个扫描范围（含子目录）"
            } else {
                "目录分析 · 双击目录进入"
            })
            .child(if self.page == Page::Issues {
                ""
            } else if self.page == Page::LargeFiles {
                "占扫描总量 / 逻辑大小 ↓"
            } else {
                "占当前目录 / 逻辑大小 ↓"
            });
        let footer = div()
            .px_4()
            .py_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .flex()
            .items_center()
            .gap_3()
            .child(
                div().flex_1().min_w_0().text_sm().truncate().child(
                    selected_path
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "选择条目查看完整路径".into()),
                ),
            )
            .child(
                Button::new("parent")
                    .label("所在目录")
                    .disabled(self.page != Page::LargeFiles || selected.is_none())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_parent();
                        cx.notify();
                    })),
            )
            .child(
                Button::new("reveal")
                    .label(if self.revealing {
                        "正在定位…"
                    } else {
                        "在 Finder 中显示"
                    })
                    .disabled(selected.is_none() || self.revealing)
                    .on_click(cx.listener(|this, _, _, cx| this.reveal_selected(cx))),
            )
            .child(
                Button::new("copy")
                    .label("复制路径")
                    .disabled(selected_path.is_none())
                    .on_click(cx.listener(move |_, _, _, cx| {
                        if let Some(path) = &selected_path {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                path.to_string_lossy().into_owned(),
                            ));
                        }
                    })),
            );

        div()
            .size_full()
            .flex()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(sidebar)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(summary)
                    .child(toolbar)
                    .child(heading)
                    .child(content)
                    .child(footer),
            )
    }
}

#[cfg(test)]
mod tests;
