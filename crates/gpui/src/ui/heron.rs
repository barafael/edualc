//! edualc's heron (`ui/heron.slint`). Over a new session's empty conversation it stands while
//! the conversation is empty, flies off when it begins, and flies in when a new one opens; it
//! faces left, so it leaves to the upper left and comes in from the upper right. The one at the
//! top of the sidebar stands still, and about once a minute, for a second or so, looks over its
//! shoulder, preens or flaps its wings, in turn. In between only a timer waits, so nothing is
//! drawn for it.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, IntoElement, ParentElement as _,
    SharedString, Styled as _, Window, div, px, svg,
};

use super::{Palette, Shell};

/// How long a flight takes, a wing beat, and the wait before it sets off to fly in (so that it
/// starts from out of sight).
const FLIGHT: Duration = Duration::from_millis(1100);
const BEAT: Duration = Duration::from_millis(110);
const APPROACH: Duration = Duration::from_millis(50);

/// The heron's size, standing, in logical pixels.
const SIZE: f32 = 64.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flight {
    Standing,
    Leaving,
    Away,
    Arriving,
}

/// The heron over the conversation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Heron {
    flight: Flight,
    /// The wing beat's frame: up, level, down, level.
    beat: u8,
    /// Which flight is under way, so that one cut short stops, and its animation starts anew.
    serial: u32,
    /// Whether it belongs here: the conversation is empty.
    wanted: bool,
}

impl Default for Heron {
    fn default() -> Self {
        Self {
            flight: Flight::Standing,
            beat: 0,
            serial: 0,
            wanted: true,
        }
    }
}

/// What the sidebar's heron does now and then, in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    LookingBack,
    Preening,
    Flapping,
}

impl Act {
    /// The wait before it, which differs, so that it doesn't tick like a clock.
    fn wait(self) -> Duration {
        Duration::from_secs(match self {
            Self::LookingBack => 61,
            Self::Preening => 83,
            Self::Flapping => 47,
        })
    }

    /// A look held, nibbles, wing beats.
    fn frame_time(self) -> Duration {
        Duration::from_millis(match self {
            Self::LookingBack => 1500,
            Self::Preening => 240,
            Self::Flapping => 110,
        })
    }

    fn frames(self) -> u8 {
        match self {
            Self::LookingBack => 1,
            Self::Preening => 6,
            Self::Flapping => 7,
        }
    }

    fn next(self) -> Self {
        match self {
            Self::LookingBack => Self::Preening,
            Self::Preening => Self::Flapping,
            Self::Flapping => Self::LookingBack,
        }
    }

    /// The drawing of its `frame`.
    fn drawing(self, frame: u8) -> &'static str {
        match self {
            Self::LookingBack => "heron-looking-back",
            // Nibbling at its back.
            Self::Preening if frame.is_multiple_of(2) => "heron-preening-1",
            Self::Preening => "heron-preening-2",
            // Wing beats: half up, up, half up, folded.
            Self::Flapping if frame.is_multiple_of(2) => "heron-flapping-half",
            Self::Flapping if frame % 4 == 1 => "heron-flapping-up",
            Self::Flapping => "heron",
        }
    }
}

/// The sidebar's heron.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mark {
    act: Act,
    /// The act's frame; none while it stands.
    frame: Option<u8>,
}

impl Default for Mark {
    fn default() -> Self {
        Self {
            act: Act::LookingBack,
            frame: None,
        }
    }
}

/// One of the heron's drawings, in Claude's clay.
fn drawing(name: &str, size: f32, palette: &Palette) -> gpui::Svg {
    svg()
        .path(SharedString::from(format!("heron/{name}.svg")))
        .size(px(size))
        .flex_none()
        .text_color(palette.claude)
}

/// Slow, then fast.
fn ease_in(delta: f32) -> f32 {
    delta * delta
}

/// Fast, then slow.
fn ease_out(delta: f32) -> f32 {
    1.0 - (1.0 - delta) * (1.0 - delta)
}

impl Shell {
    /// The conversation is empty, or not: the heron flies off, or in.
    pub(crate) fn heron_wanted(&mut self, wanted: bool, cx: &Context<Self>) {
        if self.heron.wanted == wanted {
            return;
        }
        self.heron.wanted = wanted;
        self.heron.serial = self.heron.serial.wrapping_add(1);
        self.heron.beat = 0;
        // Out of sight first, unanimated, so that it flies in from there.
        self.heron.flight = if wanted {
            Flight::Away
        } else {
            Flight::Leaving
        };
        let serial = self.heron.serial;
        cx.spawn(async move |this, cx| {
            let executor = cx.background_executor().clone();
            let timer = |wait| executor.timer(wait);
            if wanted {
                timer(APPROACH).await;
                let set = this.update(cx, |shell, cx| {
                    if shell.heron.serial == serial {
                        shell.heron.flight = Flight::Arriving;
                        cx.notify();
                    }
                });
                if set.is_err() {
                    return;
                }
            }
            let beats = FLIGHT.as_millis() / BEAT.as_millis();
            for _ in 0..beats {
                timer(BEAT).await;
                let going = this.update(cx, |shell, cx| {
                    let going = shell.heron.serial == serial;
                    if going {
                        shell.heron.beat = (shell.heron.beat + 1) % 4;
                        cx.notify();
                    }
                    going
                });
                if !matches!(going, Ok(true)) {
                    return;
                }
            }
            let landed = this.update(cx, |shell, cx| {
                if shell.heron.serial == serial {
                    shell.heron.flight = if wanted {
                        Flight::Standing
                    } else {
                        Flight::Away
                    };
                    cx.notify();
                }
            });
            if landed.is_err() {
                tracing::debug!("the window closed while the heron flew");
            }
        })
        .detach();
    }

    /// The heron over the conversation, flying `reach` (logical pixels, across and up) before
    /// it is out of sight; nothing while it is away.
    pub(crate) fn heron(&self, reach: (f32, f32), palette: &Palette) -> AnyElement {
        let heron = self.heron;
        let name = match (heron.flight, heron.beat) {
            (Flight::Away, _) => return div().into_any_element(),
            (Flight::Standing, _) => "heron",
            (_, 0) => "heron-wings-up",
            (_, 2) => "heron-wings-down",
            (_, _) => "heron-wings-level",
        };
        let bird = div()
            .relative()
            .size(px(SIZE))
            .child(drawing(name, SIZE, palette));
        let (across, up) = reach;
        match heron.flight {
            Flight::Standing | Flight::Away => bird.into_any_element(),
            // Up first, then away: it climbs at once and gathers speed sideways.
            Flight::Leaving => bird
                .with_animation(
                    ("heron-leaving", heron.serial),
                    Animation::new(FLIGHT),
                    move |bird, delta| {
                        bird.left(px(-across * ease_in(delta)))
                            .top(px(-up * ease_out(delta)))
                            .opacity(1.0 - ease_in(delta))
                    },
                )
                .into_any_element(),
            // Gliding in: it slows as it comes, and comes down last.
            Flight::Arriving => bird
                .with_animation(
                    ("heron-arriving", heron.serial),
                    Animation::new(FLIGHT),
                    move |bird, delta| {
                        bird.left(px(across * (1.0 - ease_out(delta))))
                            .top(px(-up * (1.0 - ease_in(delta))))
                            .opacity(ease_out((delta * 2.0).min(1.0)))
                    },
                )
                .into_any_element(),
        }
    }

    /// Starts the sidebar's heron's acts, for as long as the window is open.
    pub(crate) fn start_mark(cx: &Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut act = Act::LookingBack;
            loop {
                cx.background_executor().timer(act.wait()).await;
                for frame in 0..act.frames() {
                    let shown = this.update(cx, |shell, cx| {
                        shell.mark = Mark {
                            act,
                            frame: Some(frame),
                        };
                        cx.notify();
                    });
                    if shown.is_err() {
                        return;
                    }
                    cx.background_executor().timer(act.frame_time()).await;
                }
                act = act.next();
                let stood = this.update(cx, |shell, cx| {
                    shell.mark = Mark { act, frame: None };
                    cx.notify();
                });
                if stood.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// The sidebar's heron, `size` pixels square.
    pub(crate) fn heron_mark(&self, size: f32, palette: &Palette) -> impl IntoElement {
        let name = match self.mark.frame {
            None => "heron",
            Some(frame) => self.mark.act.drawing(frame),
        };
        drawing(name, size, palette)
    }
}

/// How far the heron flies before it is out of sight, from the window's size: half of it, and
/// the bird's own size.
pub(crate) fn reach(window: &Window) -> (f32, f32) {
    let size = window.viewport_size();
    (
        f32::from(size.width) / 2.0 + SIZE,
        f32::from(size.height) / 2.0 + SIZE,
    )
}
