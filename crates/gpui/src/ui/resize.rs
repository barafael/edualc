//! What is sized by dragging, as in the Slint window: the sidebar by its right edge, the
//! composer by its top edge, each kept between runs (the app keeps them in the settings); a
//! double click gives the default back. The composer can also be made taller (two thirds of
//! the column) or given the whole column, over the conversation, to see all of a long paste.

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement as _, Styled as _, Window, div, px,
};

use super::Shell;
use crate::app::Action;
use crate::present::model::ComposerSize;

/// The sidebar's widest, in logical pixels.
const WIDEST: f32 = 720.0;

/// The composer's edges are this thick, to be found with the pointer.
const EDGE: f32 = 6.0;

/// A drag under way: where it started, and the size then.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Drag {
    Sidebar { from_x: f32, from: f32 },
    Composer { from_y: f32, from: f32 },
}

impl Shell {
    /// The sidebar's width when none was chosen.
    pub(crate) fn default_sidebar_width(&self) -> f32 {
        (self.page.text_size * 18.0).max(272.0)
    }

    /// The narrowest the sidebar goes: its head's buttons and name still fit.
    fn least_sidebar_width(&self) -> f32 {
        let button = (self.page.text_size * 2.2).max(32.0);
        (4.0 * button + 24.0 + 4.0 * 8.0 + 4.0 + 2.0 * 12.0).max(200.0)
    }

    pub(crate) fn sidebar_width(&self) -> f32 {
        self.page
            .sidebar_width
            .map_or_else(|| self.default_sidebar_width(), f32::from)
    }

    /// A line of the composer, about.
    fn composer_line(&self) -> f32 {
        self.page.text_size * 1.5
    }

    /// The composer's text box's height now, about: as chosen, or fitted to the draft's lines.
    fn composer_height(&self, cx: &gpui::App) -> f32 {
        if self.page.composer_size == ComposerSize::Chosen {
            return self.page.composer_chosen;
        }
        let lines = self
            .fields
            .draft
            .read(cx)
            .value()
            .lines()
            .count()
            .clamp(2, 12);
        // A dozen lines at most: exact in `f32`.
        #[allow(clippy::cast_precision_loss)]
        let lines = lines as f32;
        lines * self.composer_line()
    }

    /// The sidebar's right edge: dragged, it sets the width; a double click gives the default
    /// back.
    pub(crate) fn sidebar_edge(&self, cx: &Context<Self>) -> AnyElement {
        let palette = self.palette();
        let dragging = matches!(self.drag, Some(Drag::Sidebar { .. }));
        let accent = palette.accent;
        let line = div().absolute().top_0().right_0().h_full().w(px(3.0));
        div()
            .id("sidebar-edge")
            .absolute()
            .top_0()
            .right(px(-EDGE / 2.0))
            .h_full()
            .w(px(EDGE))
            .cursor_col_resize()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|shell, event: &MouseDownEvent, _, cx| {
                    if event.click_count >= 2 {
                        shell.drag = None;
                        shell.page.sidebar_width = None;
                        shell.actions.send(Action::SidebarWidth(None));
                    } else {
                        shell.drag = Some(Drag::Sidebar {
                            from_x: f32::from(event.position.x),
                            from: shell.sidebar_width(),
                        });
                    }
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(if dragging {
                line.bg(accent)
            } else {
                line.hover(move |style| style.bg(accent))
            })
            .into_any_element()
    }

    /// The composer's top edge: dragged, the box takes that height and keeps it; a double click
    /// fits it to its text again.
    pub(crate) fn composer_grip(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .id("composer-grip")
            .absolute()
            .top_0()
            .left_0()
            .w_full()
            .h(px(EDGE))
            .cursor_row_resize()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|shell, event: &MouseDownEvent, _, cx| {
                    if event.click_count >= 2 {
                        shell.drag = None;
                        shell.page.composer_size = ComposerSize::Fit;
                        shell.page.composer_chosen = 0.0;
                        shell.actions.send(Action::ComposerHeight(None));
                    } else {
                        shell.drag = Some(Drag::Composer {
                            from_y: f32::from(event.position.y),
                            from: shell.composer_height(cx),
                        });
                    }
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .into_any_element()
    }

    /// The pointer moved while something is dragged: it takes the size the pointer gives it.
    pub(crate) fn drag_moved(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag else {
            return;
        };
        // Let go of outside the window: the drag ends as the pointer comes back.
        if event.pressed_button != Some(MouseButton::Left) {
            self.drag_ended(window, cx);
            return;
        }
        match drag {
            Drag::Sidebar { from_x, from } => {
                let width = (from + f32::from(event.position.x) - from_x)
                    .clamp(self.least_sidebar_width(), WIDEST);
                self.page.sidebar_width = whole(width);
            }
            Drag::Composer { from_y, from } => {
                let tallest = f32::from(window.viewport_size().height) * 0.6;
                let least = 2.0 * self.composer_line();
                self.page.composer_chosen =
                    (from + from_y - f32::from(event.position.y)).clamp(least, tallest.max(least));
                self.page.composer_size = ComposerSize::Chosen;
            }
        }
        cx.notify();
    }

    /// The button went up: a drag ends, and the size it gave is kept.
    pub(crate) fn drag_released(
        &mut self,
        _event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drag_ended(window, cx);
    }

    fn drag_ended(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        match self.drag.take() {
            Some(Drag::Sidebar { .. }) => {
                self.actions
                    .send(Action::SidebarWidth(self.page.sidebar_width));
            }
            Some(Drag::Composer { .. }) => {
                let kept = whole(self.page.composer_chosen);
                self.actions.send(Action::ComposerHeight(kept));
            }
            None => return,
        }
        cx.notify();
    }

    /// The composer made taller, or given the whole column; pressed again, back as it was.
    pub(crate) fn toggle_composer(&mut self, size: ComposerSize) {
        self.page.composer_size = if self.page.composer_size == size {
            if self.page.composer_chosen > 0.0 {
                ComposerSize::Chosen
            } else {
                ComposerSize::Fit
            }
        } else {
            size
        };
    }
}

/// `pixels` as whole logical pixels, as the settings keep them; none for what doesn't fit.
fn whole(pixels: f32) -> Option<u16> {
    let rounded = pixels.round();
    // Checked to fit before the cast.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (rounded >= 1.0 && rounded <= f32::from(u16::MAX)).then_some(rounded as u16)
}

/// Over the column, outside its layout, for the composer made taller (`ComposerSize::Tall`, the
/// lower two thirds) or given the whole column below its header (`Full`).
pub(crate) fn over_column(
    size: ComposerSize,
    header: f32,
    fill: gpui::Hsla,
    composer: AnyElement,
) -> AnyElement {
    let cover = div()
        .bg(fill)
        .border_t_1()
        .border_color(gpui::black().opacity(0.3))
        .absolute()
        .left_0()
        .right_0()
        .bottom_0()
        .flex()
        .flex_col()
        .pt(px(12.0));
    match size {
        ComposerSize::Full => cover.top(px(header)),
        ComposerSize::Tall | ComposerSize::Fit | ComposerSize::Chosen => {
            cover.top(gpui::relative(1.0 / 3.0))
        }
    }
    .child(composer)
    .into_any_element()
}
