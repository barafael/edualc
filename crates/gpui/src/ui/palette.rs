//! The window's colours, as the Slint window's (`ui/theme.slint`): Gruvbox, or plain greys with
//! a blue accent; each dark or light as the desktop asks. What each is for is told there.

use gpui::{Hsla, WindowAppearance, rgb, rgba};

use crate::present::Look;

/// One look's colours, dark or light.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Palette {
    pub(crate) surface: Hsla,
    pub(crate) surface_raised: Hsla,
    pub(crate) card: Hsla,
    pub(crate) dim: Hsla,
    pub(crate) bubble: Hsla,
    pub(crate) code: Hsla,
    pub(crate) field: Hsla,
    pub(crate) divider: Hsla,
    pub(crate) control_border: Hsla,
    pub(crate) text: Hsla,
    pub(crate) text_secondary: Hsla,
    pub(crate) text_tertiary: Hsla,
    pub(crate) text_faint: Hsla,
    pub(crate) accent: Hsla,
    pub(crate) on_accent: Hsla,
    pub(crate) link: Hsla,
    pub(crate) selection: Hsla,
    pub(crate) claude: Hsla,
    pub(crate) success: Hsla,
    pub(crate) warning: Hsla,
    pub(crate) warning_text: Hsla,
    pub(crate) danger: Hsla,
    pub(crate) danger_text: Hsla,
    pub(crate) info: Hsla,
    added: Hsla,
}

/// Whether the desktop asks for dark.
pub(crate) fn is_dark(appearance: WindowAppearance) -> bool {
    match appearance {
        WindowAppearance::Dark | WindowAppearance::VibrantDark => true,
        WindowAppearance::Light | WindowAppearance::VibrantLight => false,
    }
}

fn hex(value: u32) -> Hsla {
    rgb(value).into()
}

fn hexa(value: u32) -> Hsla {
    rgba(value).into()
}

impl Palette {
    pub(crate) fn of(look: Look, dark: bool) -> Self {
        match (look, dark) {
            (Look::Gruvbox, true) => Self {
                surface: hex(0x28_2828),
                surface_raised: hex(0x1d_2021),
                card: hex(0x32_302f),
                dim: hexa(0x0000_0044),
                bubble: hex(0x3c_3836),
                code: hex(0x1d_2021),
                field: hex(0x32_302f),
                divider: hex(0x3c_3836),
                control_border: hex(0x83_7769),
                text: hex(0xeb_dbb2),
                text_secondary: hex(0xbd_ae93),
                text_tertiary: hex(0xa8_9984),
                text_faint: hex(0x92_8374),
                accent: hex(0xfe_8019),
                on_accent: hex(0x1d_2021),
                link: hex(0x83_a598),
                selection: hex(0x50_4945),
                claude: hex(0xfe_8019),
                success: hex(0xb8_bb26),
                warning: hex(0xfa_bd2f),
                warning_text: hex(0xfa_bd2f),
                danger: hex(0xfb_4934),
                danger_text: hex(0xfb_5841),
                info: hex(0x8e_c07c),
                added: hex(0xb8_bb26),
            },
            (Look::Gruvbox, false) => Self {
                surface: hex(0xfb_f1c7),
                surface_raised: hex(0xf2_e5bc),
                card: hex(0xf9_f5d7),
                dim: hexa(0x0000_0022),
                bubble: hex(0xeb_dbb2),
                code: hex(0xf2_e5bc),
                field: hex(0xf9_f5d7),
                divider: hex(0xd5_c4a1),
                control_border: hex(0x96_8977),
                text: hex(0x3c_3836),
                text_secondary: hex(0x50_4945),
                text_tertiary: hex(0x66_5c54),
                text_faint: hex(0x7c_6f64),
                accent: hex(0xaf_3a03),
                on_accent: hex(0xfb_f1c7),
                link: hex(0x07_6678),
                selection: hex(0xd5_c4a1),
                claude: hex(0xd6_5d0e),
                success: hex(0x73_6f10),
                warning: hex(0xb5_7614),
                warning_text: hex(0x92_6219),
                danger: hex(0x9d_0006),
                danger_text: hex(0x9d_0006),
                info: hex(0x41_7756),
                added: hex(0x79_740e),
            },
            (Look::Plain, true) => Self {
                surface: hex(0x20_2020),
                surface_raised: hex(0x18_1818),
                card: hex(0x26_2626),
                dim: hexa(0x0000_0044),
                bubble: hex(0x2f_2f2f),
                code: hex(0x18_1818),
                field: hex(0x26_2626),
                divider: hex(0x2f_2f2f),
                control_border: hex(0x7a_7a7a),
                text: hex(0xe8_e8e8),
                text_secondary: hex(0xc4_c4c4),
                text_tertiary: hex(0xad_adad),
                text_faint: hex(0x95_9595),
                accent: hex(0x6e_a8fe),
                on_accent: hex(0x0b_0d10),
                link: hex(0x8a_b4f8),
                selection: hex(0x2d_4f7c),
                claude: hex(0xd9_7757),
                success: hex(0x6b_c46d),
                warning: hex(0xe5_b443),
                warning_text: hex(0xe5_b443),
                danger: hex(0xf0_645c),
                danger_text: hex(0xf4_7b74),
                info: hex(0x5b_b8d6),
                added: hex(0x6b_c46d),
            },
            (Look::Plain, false) => Self {
                surface: hex(0xff_ffff),
                surface_raised: hex(0xf3_f3f3),
                card: hex(0xfa_fafa),
                dim: hexa(0x0000_0022),
                bubble: hex(0xec_ecec),
                code: hex(0xf5_f5f5),
                field: hex(0xff_ffff),
                divider: hex(0xe2_e2e2),
                control_border: hex(0x85_8585),
                text: hex(0x1c_1c1c),
                text_secondary: hex(0x3d_3d3d),
                text_tertiary: hex(0x55_5555),
                text_faint: hex(0x6b_6b6b),
                accent: hex(0x1f_5fd1),
                on_accent: hex(0xff_ffff),
                link: hex(0x1a_56c4),
                selection: hex(0xc8_dbff),
                claude: hex(0xb8_532f),
                success: hex(0x1e_7b34),
                warning: hex(0xb2_6a00),
                warning_text: hex(0x8a_5300),
                danger: hex(0xc0_281e),
                danger_text: hex(0xb3_261e),
                info: hex(0x0b_6e78),
                added: hex(0x1e_7b34),
            },
        }
    }

    /// Fills laid over a surface.
    pub(crate) fn hover(&self) -> Hsla {
        self.text.opacity(0.07)
    }

    pub(crate) fn selected(&self) -> Hsla {
        self.accent.opacity(0.16)
    }

    /// A diff's line, behind it (faint).
    pub(crate) fn added(&self) -> Hsla {
        self.added.opacity(0.16)
    }

    pub(crate) fn removed(&self) -> Hsla {
        self.danger.opacity(0.16)
    }

    /// A share's colour, warming as it fills: a warning from 70%, danger from 90%.
    pub(crate) fn fill(&self, fraction: f32) -> Hsla {
        if fraction >= 0.9 {
            self.danger
        } else if fraction >= 0.7 {
            self.warning
        } else {
            self.info
        }
    }

    /// These colours, for gpui-component's widgets (the text fields, buttons, menus, scroll
    /// bars, Markdown).
    pub(crate) fn paint(&self, colors: &mut gpui_component::ThemeColor) {
        colors.background = self.surface;
        colors.foreground = self.text;
        colors.border = self.divider;
        colors.input = self.control_border;
        colors.ring = self.accent;
        colors.caret = self.accent;
        colors.selection = self.selection;
        colors.muted = self.code;
        colors.muted_foreground = self.text_tertiary;
        colors.accent = self.hover();
        colors.accent_foreground = self.text;
        colors.primary = self.accent;
        colors.primary_hover = self.accent.opacity(0.9);
        colors.primary_active = self.accent.opacity(0.8);
        colors.primary_foreground = self.on_accent;
        colors.secondary = self.bubble;
        colors.secondary_hover = self.selection;
        colors.secondary_active = self.selection;
        colors.secondary_foreground = self.text;
        colors.danger = self.danger;
        colors.danger_hover = self.danger.opacity(0.9);
        colors.danger_active = self.danger.opacity(0.8);
        colors.danger_foreground = self.on_accent;
        colors.popover = self.surface_raised;
        colors.popover_foreground = self.text;
        colors.link = self.link;
        colors.link_hover = self.link;
        colors.link_active = self.link;
        colors.list = self.surface_raised;
        colors.list_hover = self.hover();
        colors.list_active = self.selected();
        colors.list_active_border = self.accent;
        colors.scrollbar = gpui::transparent_black();
        colors.scrollbar_thumb = self.text.opacity(0.25);
        colors.scrollbar_thumb_hover = self.text.opacity(0.4);
        colors.sidebar = self.surface_raised;
        colors.sidebar_foreground = self.text;
        colors.sidebar_border = self.divider;
        colors.table_head = self.bubble;
        colors.table_head_foreground = self.text;
        colors.table_hover = self.hover();
        colors.table_row_border = self.divider;
        colors.overlay = self.dim;
        colors.title_bar = self.surface_raised;
        colors.success = self.success;
        colors.warning = self.warning;
        colors.info = self.info;
    }
}
