mod analysis;
mod models;
mod platform;
mod scanner;
mod ui;

use gpui_kit::*;

#[cfg(target_os = "macos")]
#[global_allocator]
static ALLOCATOR: platform::LargeAllocator = platform::LargeAllocator;

fn main() {
    application().with_assets(assets::Assets).run(|cx| {
        init(cx);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1100.0), px(720.0)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("Clean".into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        open_window(options, cx, |_, cx| cx.new(ui::AppView::new))
            .expect("Failed to open the application window");
        cx.activate(true);
    });
}
