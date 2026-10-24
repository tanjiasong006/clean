mod ui;

use gpui_kit::*;

fn main() {
    application().with_assets(assets::Assets).run(|cx| {
        init(cx);

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

        open_window(options, cx, |_, cx| cx.new(|_| ui::AppView))
            .expect("Failed to open the application window");
        cx.activate(true);
    });
}
