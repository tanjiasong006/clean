use gpui_kit::component::{ActiveTheme, WindowExt, button::Button};
use gpui_kit::*;

pub struct AppView;

impl Render for AppView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(div().text_3xl().child("Clean"))
            .child("磁盘空间分析工具")
            .child("应用框架已就绪，目录扫描功能将在下一阶段加入。")
            .child(
                Button::new("about")
                    .label("关于 Clean")
                    .on_click(|_, window, cx| {
                        window.open_dialog(cx, |dialog, _, _| {
                            dialog
                                .title("关于 Clean")
                                .child("使用 Rust、GPUI 和 GPUI Kit 构建，优先支持 macOS。")
                        });
                    }),
            )
    }
}
