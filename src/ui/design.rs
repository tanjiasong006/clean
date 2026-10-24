use gpui_kit::{
    ElementId, Hsla, Styled,
    component::{Disableable, StyledExt, Theme, ThemeMode, button::Button},
    prelude::FluentBuilder,
    px, rgb,
};

use crate::analysis::Category;

pub(super) mod type_size {
    pub const CAPTION: f32 = 11.0;
    pub const LABEL: f32 = 12.0;
    pub const BODY: f32 = 13.0;
    pub const HEADING: f32 = 14.0;
    pub const TITLE: f32 = 18.0;
    pub const DISPLAY: f32 = 26.0;
}

pub(super) mod layout {
    pub const SIDEBAR: f32 = 216.0;
    pub const INSPECTOR: f32 = 300.0;
    pub const NAV_ROW: f32 = 28.0;
    pub const ROW: f32 = 34.0;
    pub const ROW_COMPACT: f32 = 28.0;
    pub const TOOLBAR: f32 = 46.0;
    pub const CONTENT_TOOLBAR: f32 = 36.0;
    pub const TABLE_HEAD: f32 = 26.0;
    pub const STATUS: f32 = 22.0;
}

pub(super) mod radius {
    pub const SMALL: f32 = 4.0;
    pub const CONTROL: f32 = 6.0;
    pub const LARGE: f32 = 12.0;
}

#[derive(Clone, Copy)]
pub(super) struct CategoryColors {
    colors: [Hsla; 10],
}

impl CategoryColors {
    pub fn color(&self, category: Category) -> Hsla {
        self.colors[category.index()]
    }
}

#[derive(Clone, Copy)]
pub(super) struct Palette {
    pub sidebar: Hsla,
    pub main: Hsla,
    pub toolbar: Hsla,
    pub elevated: Hsla,
    pub recessed: Hsla,
    pub text_primary: Hsla,
    pub text_secondary: Hsla,
    pub text_tertiary: Hsla,
    pub text_disabled: Hsla,
    pub accent: Hsla,
    pub accent_hover: Hsla,
    pub accent_pressed: Hsla,
    pub on_accent: Hsla,
    pub selected: Hsla,
    pub on_selected: Hsla,
    pub selected_border: Hsla,
    pub focus: Hsla,
    pub hover: Hsla,
    pub pressed: Hsla,
    pub border: Hsla,
    pub folder: Hsla,
    pub meter: Hsla,
    pub bar_track: Hsla,
    pub bar_fill: Hsla,
    pub data_accent: Hsla,
    pub warning: Hsla,
    pub error: Hsla,
    pub categories: CategoryColors,
}

impl Palette {
    pub fn for_theme(theme: &Theme) -> Self {
        Self::for_mode(theme.mode)
    }

    fn for_mode(mode: ThemeMode) -> Self {
        let color = |value| -> Hsla { rgb(value).into() };
        match mode {
            ThemeMode::Dark => Self {
                sidebar: color(0x262a32),
                main: color(0x2b2f37),
                toolbar: color(0x2e323b),
                elevated: color(0x353a44),
                recessed: color(0x282c34),
                text_primary: color(0xe9edf5),
                text_secondary: color(0xb8c0ce),
                text_tertiary: color(0x9ea8b9),
                text_disabled: color(0x677182),
                accent: color(0x8dacf0),
                accent_hover: color(0xa0baf4),
                accent_pressed: color(0x7b9bdd),
                on_accent: color(0x1e2b46),
                selected: color(0x333c52),
                on_selected: color(0xdfe8fa),
                selected_border: color(0x8dacf0).opacity(0.6),
                focus: color(0xa9c2ff),
                hover: color(0x303640),
                pressed: color(0x373f4d),
                border: color(0x454d5b).opacity(0.45),
                folder: color(0x8496b3),
                meter: color(0x8e9db6),
                bar_track: color(0x3a404c),
                bar_fill: color(0x748cba),
                data_accent: color(0x7291d3),
                warning: color(0xe2bd7f),
                error: color(0xf09b9f),
                categories: CategoryColors {
                    colors: [
                        color(0x8ea9dd),
                        color(0xc4a678),
                        color(0x93bf9e),
                        color(0xb0a0d8),
                        color(0xd0a0ba),
                        color(0x85bdbd),
                        color(0xd3a17c),
                        color(0xa0abbf),
                        color(0xcd9a9f),
                        color(0x9aa4b4),
                    ],
                },
            },
            ThemeMode::Light => Self {
                sidebar: color(0xf1f3f7),
                main: color(0xfafbfd),
                toolbar: color(0xf6f7fa),
                elevated: color(0xffffff),
                recessed: color(0xf4f6f9),
                text_primary: color(0x252e3d),
                text_secondary: color(0x4d5b70),
                text_tertiary: color(0x5e6d82),
                text_disabled: color(0x98a2b1),
                accent: color(0x486cb2),
                accent_hover: color(0x3d5f9f),
                accent_pressed: color(0x344f86),
                on_accent: color(0xf8faff),
                selected: color(0xe9eef9),
                on_selected: color(0x2c4a7a),
                selected_border: color(0x486cb2).opacity(0.55),
                focus: color(0x5277c0),
                hover: color(0xedf0f5),
                pressed: color(0xe4e9f1),
                border: color(0xcbd3df).opacity(0.5),
                folder: color(0x6f819c),
                meter: color(0x7b8da9),
                bar_track: color(0xe6ebf2),
                bar_fill: color(0x6f88ad),
                data_accent: color(0x486cb2),
                warning: color(0x93621d),
                error: color(0xb84e5a),
                categories: CategoryColors {
                    colors: [
                        color(0x4f6fae),
                        color(0x8a6a3c),
                        color(0x4f7f58),
                        color(0x6f5a9e),
                        color(0xa05f78),
                        color(0x3f7b7b),
                        color(0xa16a3a),
                        color(0x5f6c80),
                        color(0x8f5057),
                        color(0x6b7482),
                    ],
                },
            },
        }
    }
}

pub(super) fn apply(theme: &mut Theme) {
    let p = Palette::for_theme(theme);
    theme.font_family = ".SystemUIFont".into();
    // Keep the rem grid stable: GPUI Kit also derives control spacing from it.
    theme.font_size = px(16.0);
    theme.radius = px(radius::CONTROL);
    theme.radius_lg = px(radius::LARGE);
    theme.shadow = false;
    theme.background = p.main;
    theme.foreground = p.text_primary;
    theme.muted = p.hover;
    theme.muted_foreground = p.text_secondary;
    theme.primary = p.accent;
    theme.primary_hover = p.accent_hover;
    theme.primary_active = p.accent_pressed;
    theme.primary_foreground = p.on_accent;
    theme.button_primary = p.accent;
    theme.button_primary_hover = p.accent_hover;
    theme.button_primary_active = p.accent_pressed;
    theme.button_primary_foreground = p.on_accent;
    theme.secondary = p.elevated;
    theme.secondary_hover = p.hover;
    theme.secondary_active = p.selected;
    theme.secondary_foreground = p.text_secondary;
    theme.button_secondary = p.elevated;
    theme.button_secondary_hover = p.hover;
    theme.button_secondary_active = p.pressed;
    theme.button_secondary_foreground = p.text_primary;
    theme.button = p.elevated;
    theme.button_hover = p.hover;
    theme.button_active = p.pressed;
    theme.button_foreground = p.text_primary;
    theme.accent = p.hover;
    theme.accent_foreground = p.text_primary;
    theme.ring = p.focus;
    theme.selection = p.selected;
    theme.border = p.border;
    theme.input = p.border;
    theme.sidebar = p.sidebar;
    theme.sidebar_foreground = p.text_secondary;
    theme.sidebar_accent = p.selected;
    theme.sidebar_accent_foreground = p.on_selected;
    theme.sidebar_border = p.border;
    theme.sidebar_primary = p.accent;
    theme.sidebar_primary_foreground = p.on_accent;
    theme.title_bar = p.toolbar;
    theme.title_bar_border = p.border;
    theme.status_bar = p.recessed;
    theme.status_bar_border = p.border;
    theme.popover = p.elevated;
    theme.popover_foreground = p.text_primary;
    theme.colors.list = p.main;
    theme.list_head = p.recessed;
    theme.list_hover = p.hover;
    theme.list_active = p.selected;
    theme.list_active_border = p.selected_border;
    theme.table = p.main;
    theme.table_head = p.recessed;
    theme.table_head_foreground = p.text_tertiary;
    theme.table_hover = p.hover;
    theme.table_active = p.selected;
    theme.table_active_border = p.selected_border;
    theme.table_row_border = p.border;
    theme.scrollbar = theme.transparent;
    theme.scrollbar_thumb = p.text_disabled;
    theme.scrollbar_thumb_hover = p.text_tertiary;
    theme.link = p.accent;
    theme.link_hover = p.accent_hover;
    theme.link_active = p.accent_pressed;
    theme.chart_2 = p.data_accent;
    theme.progress_bar = p.bar_fill;
    theme.yellow = p.warning;
    theme.red = p.error;
}

pub(super) fn button(id: impl Into<ElementId>, disabled: bool, theme: &Theme) -> Button {
    Button::new(id)
        .font_medium()
        .rounded(px(radius::CONTROL))
        .disabled(disabled)
        .when(disabled, |button| {
            button.text_color(Palette::for_theme(theme).text_disabled)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(color: Hsla) -> f32 {
        let color = color.to_rgb();
        let linear = |value: f32| {
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
    }

    fn contrast(a: Hsla, b: Hsla) -> f32 {
        let (a, b) = (luminance(a), luminance(b));
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    #[test]
    fn text_levels_remain_readable_and_ordered_on_all_surfaces() {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            let p = Palette::for_mode(mode);
            for surface in [p.sidebar, p.main, p.toolbar, p.elevated, p.recessed] {
                let levels = [
                    p.text_primary,
                    p.text_secondary,
                    p.text_tertiary,
                    p.text_disabled,
                ]
                .map(|ink| contrast(ink, surface));
                assert!(
                    levels[..3].iter().all(|ratio| *ratio >= 4.5),
                    "{mode:?}: {levels:?}"
                );
                assert!(levels.windows(2).all(|pair| pair[0] > pair[1]));
            }
        }
    }

    #[test]
    fn accent_states_and_selected_text_have_accessible_contrast() {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            let p = Palette::for_mode(mode);
            for state in [p.accent, p.accent_hover, p.accent_pressed] {
                assert!(contrast(p.on_accent, state) >= 4.5, "{mode:?}");
            }
            assert!(contrast(p.on_selected, p.selected) >= 4.5);
            assert!(contrast(p.focus, p.main) >= 3.0);
        }
    }

    #[test]
    fn selection_tint_stays_subtle_against_the_surface() {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            let p = Palette::for_mode(mode);
            let tint = contrast(p.selected, p.main);
            assert!(tint > 1.05 && tint < 1.35, "{mode:?}: {tint}");
        }
    }

    #[test]
    fn category_colors_stay_visible_and_muted_on_both_modes() {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            let p = Palette::for_mode(mode);
            for category in Category::ALL {
                let color = p.categories.color(category);
                let ratio = contrast(color, p.main);
                assert!(ratio >= 3.0, "{mode:?} {category:?}: {ratio}");
                assert!(ratio <= 8.0, "{mode:?} {category:?}: {ratio}");
            }
        }
    }

    #[test]
    fn bars_stay_legible_without_outshining_primary_text() {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            let p = Palette::for_mode(mode);
            assert!(contrast(p.bar_fill, p.bar_track) >= 3.0, "{mode:?}");
            assert!(
                contrast(p.bar_fill, p.main) < contrast(p.text_primary, p.main),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn applying_palette_preserves_mode_and_spacing_across_switches() {
        let mut theme = Theme::default();
        for mode in [ThemeMode::Dark, ThemeMode::Light, ThemeMode::Dark] {
            theme.mode = mode;
            apply(&mut theme);
            let p = Palette::for_mode(mode);
            assert_eq!(theme.mode, mode);
            assert_eq!(theme.background, p.main);
            assert_eq!(theme.button_primary, p.accent);
            assert_eq!(theme.sidebar_accent, p.selected);
            assert_eq!(
                theme.font_family,
                gpui_kit::SharedString::from(".SystemUIFont")
            );
            assert_eq!(theme.font_size, px(16.0));
            assert_eq!(theme.radius, px(radius::CONTROL));
        }
    }
}
