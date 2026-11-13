//! Code coloured on a thread of its own, so the window never waits for it: a session's history
//! or the look's colours changing colours every block shown, at about 0.1 ms a line.
//!
//! The thread owns the [`Highlighter`]; the frontend orders a block coloured ([`Order`]) and is
//! woken (`Event::CodeColoured`) to take what is [`Coloured`]. The browser demo has no threads:
//! there the orders are coloured as the frontend takes what is coloured, on the page's thread. Of the orders for one block
//! waiting, the last is coloured; the others were of code since grown. A block that grows is
//! coloured carrying on from how far it was ([`Progress`]), so streaming costs its new lines.
//! The window is woken at most once a frame's time ([`MIN_WAKE`]), and less often after long
//! code ([`pause_after`]), whatever arrives in between coloured by then: the last state of each
//! block is always coloured and sent.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

use crate::app::Event;
use crate::generated::SyntaxColours;
use crate::highlight::{Highlighter, Progress};

/// The least time between two wakes of the window, its code redrawn on each; and so the most
/// colouring is done before the window has some.
const MIN_WAKE: Duration = Duration::from_millis(16);

/// What the window takes to lay out a line of code, about: a block's text is laid out whole
/// each time it changes. Measured on Slint 1.19's femtovg renderer, on a desktop, at 3000 lines of
/// Rust: about 0.07 ms a line (0.055 ms plain), against 9 ms in all to parse its Markdown.
const LAYOUT_PER_LINE: Duration = Duration::from_micros(70);

/// How many blocks keep their [`Progress`]: those growing now, a few.
const KEPT_PROGRESS: usize = 8;

/// Where a block of code is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Place {
    /// Block `block` of the transcript's row `row`.
    Entry { row: usize, block: usize },
    /// Block `block` of the side panel's exchange `exchange`.
    Side { exchange: usize, block: usize },
    /// The plan's section `section`.
    Plan { section: usize },
}

/// A block of code to colour, as it is now at `place`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Job {
    /// The frontend's places and colours as they were when ordered: an order or a result of an
    /// earlier generation is of rows or colours gone.
    pub generation: u64,
    pub place: Place,
    pub lang: String,
    pub raw: String,
}

/// What the frontend asks of the thread.
pub(crate) enum Order {
    Colour(Job),
    /// The look's colours changed: what is coloured from now is in these.
    Colours {
        generation: u64,
        colours: SyntaxColours,
    },
}

/// A block of code coloured, as it was ordered.
#[derive(Debug, Clone)]
pub(crate) struct Coloured {
    pub job: Job,
    pub text: slint::StyledText,
}

/// Where the frontend orders code coloured.
pub(crate) struct Orders {
    orders: Sender<Order>,
    /// In the browser demo, what wakes the window to colour what it ordered.
    #[cfg(target_arch = "wasm32")]
    events: async_channel::Sender<Event>,
}

impl Orders {
    pub(crate) fn send(&self, order: Order) {
        if self.orders.send(order).is_err() {
            tracing::debug!("code's thread is gone: code is shown plain");
        }
        #[cfg(target_arch = "wasm32")]
        if self.events.try_send(Event::CodeColoured).is_err() {
            tracing::debug!("the window is gone: code is shown plain");
        }
    }
}

/// Where the frontend takes what is coloured, when it is woken.
pub(crate) struct Colourist {
    coloured: Receiver<Coloured>,
    /// In the browser demo, the colouring itself, done as the frontend takes what is coloured.
    #[cfg(target_arch = "wasm32")]
    inline: Inline,
}

impl Colourist {
    /// What is coloured since the frontend last took it, in the order it was coloured.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn take(&self) -> Vec<Coloured> {
        self.coloured.try_iter().collect()
    }

    /// What is ordered, coloured now, on the page's one thread.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn take(&mut self) -> Vec<Coloured> {
        self.inline.colour();
        self.coloured.try_iter().collect()
    }
}

/// Starts the thread, the window woken through `events` when it has coloured code: the orders
/// go by [`Orders`], and the results come by [`Colourist`]. The thread ends with the frontend,
/// as the orders' sender is dropped. If it can't start, code stays plain.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn spawn(
    colours: &SyntaxColours,
    events: async_channel::Sender<Event>,
) -> (Orders, Colourist) {
    let (orders, ordered) = std::sync::mpsc::channel();
    let (done, coloured) = std::sync::mpsc::channel();
    let colours = colours.clone();
    let started = std::thread::Builder::new()
        .name("colourist".into())
        .spawn(move || {
            run(&mut Highlighter::new(&colours), &ordered, &done, || {
                events.send_blocking(Event::CodeColoured).is_ok()
            });
        });
    if let Err(error) = started {
        tracing::warn!(%error, "code will be shown plain: its thread didn't start");
    }
    (Orders { orders }, Colourist { coloured })
}

/// The browser demo's: what is ordered is coloured as the frontend takes what is coloured.
#[cfg(target_arch = "wasm32")]
pub(crate) fn spawn(
    colours: &SyntaxColours,
    events: async_channel::Sender<Event>,
) -> (Orders, Colourist) {
    let (orders, ordered) = std::sync::mpsc::channel();
    let (done, coloured) = std::sync::mpsc::channel();
    let inline = Inline {
        highlighter: Highlighter::new(colours),
        ordered,
        done,
        queue: Queue::default(),
        kept: Kept::default(),
    };
    (Orders { orders, events }, Colourist { coloured, inline })
}

/// The colouring, on the page's one thread.
#[cfg(target_arch = "wasm32")]
struct Inline {
    highlighter: Highlighter,
    ordered: Receiver<Order>,
    done: Sender<Coloured>,
    queue: Queue,
    kept: Kept,
}

#[cfg(target_arch = "wasm32")]
impl Inline {
    /// Colours all that is ordered, the last order of each block.
    fn colour(&mut self) {
        while let Ok(order) = self.ordered.try_recv() {
            self.queue
                .take(order, &mut self.highlighter, &mut self.kept);
        }
        while let Some(job) = self.queue.next() {
            let mut progress = self.kept.take(job.place);
            let text = self.highlighter.code(&job.lang, &job.raw, &mut progress);
            self.kept.keep(job.place, progress);
            if self.done.send(Coloured { job, text }).is_err() {
                return;
            }
        }
    }
}

/// Colours what is ordered until the orders end, or `wake` says no one is woken any more.
#[cfg(not(target_arch = "wasm32"))]
fn run(
    highlighter: &mut Highlighter,
    orders: &Receiver<Order>,
    done: &Sender<Coloured>,
    mut wake: impl FnMut() -> bool,
) {
    let mut queue = Queue::default();
    let mut kept = Kept::default();
    let mut last_wake: Option<Instant> = None;
    let mut pause = MIN_WAKE;
    loop {
        if queue.is_empty() {
            match orders.recv() {
                Ok(order) => queue.take(order, highlighter, &mut kept),
                Err(_) => return,
            }
        }
        // Ended, the orders already taken are coloured still: their results are sent while
        // someone takes them.
        while let Ok(order) = orders.try_recv() {
            queue.take(order, highlighter, &mut kept);
        }
        let began = Instant::now();
        let mut longest = 0;
        while let Some(job) = queue.next() {
            let mut progress = kept.take(job.place);
            let lines = job.raw.lines().count();
            let _span = tracing::debug_span!("colour", lang = %job.lang, lines).entered();
            let text = highlighter.code(&job.lang, &job.raw, &mut progress);
            kept.keep(job.place, progress);
            longest = longest.max(lines);
            if done.send(Coloured { job, text }).is_err() {
                return;
            }
            if began.elapsed() >= MIN_WAKE {
                break;
            }
        }
        if let Some(since) = last_wake.map(|last| last.elapsed())
            && since < pause
        {
            // What is ordered meanwhile waits in the channel, the last of each block taken.
            std::thread::sleep(pause.saturating_sub(since));
        }
        if !wake() {
            return;
        }
        last_wake = Some(Instant::now());
        pause = pause_after(longest);
    }
}

/// How long the window is let be after it is woken with code of `lines` lines at most: it lays
/// them out for about a third of that time, so that it is busy with code at most a third of the
/// time while a long block streams in. A frame's time, for short code.
#[cfg(not(target_arch = "wasm32"))]
fn pause_after(lines: usize) -> Duration {
    let lines = u32::try_from(lines).unwrap_or(u32::MAX);
    MIN_WAKE.max(LAYOUT_PER_LINE.saturating_mul(lines).saturating_mul(3))
}

/// The jobs waiting, in the order they were first ordered; one for each place, the last.
#[derive(Default)]
struct Queue {
    generation: u64,
    jobs: Vec<Option<Job>>,
    /// Where each place's job is in `jobs`.
    at: HashMap<Place, usize>,
    /// How many of `jobs` are taken.
    next: usize,
}

impl Queue {
    fn is_empty(&self) -> bool {
        self.next >= self.jobs.len()
    }

    fn take(&mut self, order: Order, highlighter: &mut Highlighter, kept: &mut Kept) {
        match order {
            Order::Colours {
                generation,
                colours,
            } => {
                if generation < self.generation {
                    return;
                }
                highlighter.set_colours(&colours);
                // Each block's progress is in the colours gone.
                *kept = Kept::default();
                self.renew(generation);
            }
            Order::Colour(job) => {
                if job.generation < self.generation {
                    return;
                }
                if job.generation > self.generation {
                    // The rows are others: what was known of their places is of no use.
                    *kept = Kept::default();
                    self.renew(job.generation);
                }
                match self.at.get(&job.place) {
                    Some(&index) if index >= self.next => {
                        if let Some(waiting) = self.jobs.get_mut(index) {
                            *waiting = Some(job);
                        }
                    }
                    Some(_) | None => {
                        self.at.insert(job.place, self.jobs.len());
                        self.jobs.push(Some(job));
                    }
                }
            }
        }
    }

    /// The jobs of `generation` from now: those waiting are of another.
    fn renew(&mut self, generation: u64) {
        self.generation = generation;
        self.jobs.clear();
        self.at.clear();
        self.next = 0;
    }

    fn next(&mut self) -> Option<Job> {
        while let Some(slot) = self.jobs.get_mut(self.next) {
            self.next += 1;
            if let Some(job) = slot.take() {
                self.at.remove(&job.place);
                if self.is_empty() {
                    self.jobs.clear();
                    self.next = 0;
                }
                return Some(job);
            }
        }
        None
    }
}

/// How far the blocks coloured last are coloured, to carry on from as they grow.
#[derive(Default)]
struct Kept {
    /// The least recently coloured first.
    blocks: Vec<(Place, Progress)>,
}

impl Kept {
    fn take(&mut self, place: Place) -> Option<Progress> {
        let index = self.blocks.iter().position(|(kept, _)| *kept == place)?;
        Some(self.blocks.remove(index).1)
    }

    fn keep(&mut self, place: Place, progress: Option<Progress>) {
        if let Some(progress) = progress {
            self.blocks.push((place, progress));
        }
        if self.blocks.len() > KEPT_PROGRESS {
            self.blocks.remove(0);
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    fn colours() -> SyntaxColours {
        SyntaxColours {
            comment: slint::Color::from_rgb_u8(0x95, 0x86, 0x76),
            keyword: slint::Color::from_rgb_u8(0xfb, 0x49, 0x34),
            string: slint::Color::from_rgb_u8(0xb8, 0xbb, 0x26),
            constant: slint::Color::from_rgb_u8(0xd3, 0x86, 0x9b),
            type_name: slint::Color::from_rgb_u8(0xfa, 0xbd, 0x2f),
            function: slint::Color::from_rgb_u8(0x8e, 0xc0, 0x7c),
            inserted: slint::Color::from_rgb_u8(0xb8, 0xbb, 0x26),
            deleted: slint::Color::from_rgb_u8(0xfb, 0x58, 0x41),
        }
    }

    fn job(generation: u64, place: Place, lang: &str, raw: &str) -> Order {
        Order::Colour(Job {
            generation,
            place,
            lang: lang.into(),
            raw: raw.into(),
        })
    }

    const FIRST: Place = Place::Entry { row: 0, block: 0 };
    const SECOND: Place = Place::Entry { row: 1, block: 0 };

    /// Runs the thread's loop on `orders`, ended as they end: what it coloured.
    fn colour_all(orders: Vec<Order>) -> Vec<Coloured> {
        let (send, ordered) = std::sync::mpsc::channel();
        let (done, coloured) = std::sync::mpsc::channel();
        for order in orders {
            send.send(order).expect("the receiver is here");
        }
        drop(send);
        run(&mut Highlighter::new(&colours()), &ordered, &done, || true);
        coloured.try_iter().collect()
    }

    fn debug(text: &slint::StyledText) -> String {
        format!("{text:?}")
    }

    /// Results cross from the thread to the window's.
    #[test]
    fn what_is_coloured_can_be_sent() {
        fn sendable<T: Send>() {}
        sendable::<Coloured>();
        sendable::<Order>();
    }

    #[test]
    fn the_last_order_for_a_block_is_coloured() {
        let coloured = colour_all(vec![
            job(0, FIRST, "rust", "fn"),
            job(0, SECOND, "py", "x = 1"),
            job(0, FIRST, "rust", "fn f"),
            job(0, FIRST, "rust", "fn f() {}"),
        ]);
        let made: Vec<_> = coloured
            .iter()
            .map(|done| (done.job.place, done.job.raw.as_str()))
            .collect();
        assert_eq!(made, vec![(FIRST, "fn f() {}"), (SECOND, "x = 1")]);
    }

    #[test]
    fn orders_of_an_earlier_generation_are_dropped() {
        let coloured = colour_all(vec![
            job(1, FIRST, "rust", "fn a"),
            job(2, SECOND, "rust", "fn b"),
            job(1, FIRST, "rust", "fn c"),
        ]);
        let made: Vec<_> = coloured.iter().map(|done| done.job.raw.as_str()).collect();
        assert_eq!(made, vec!["fn b"]);
    }

    #[test]
    fn new_colours_colour_what_follows() {
        let mut other = colours();
        other.keyword = slint::Color::from_rgb_u8(0x01, 0x02, 0x03);
        let coloured = colour_all(vec![
            job(0, FIRST, "rust", "fn a"),
            Order::Colours {
                generation: 1,
                colours: other.clone(),
            },
            job(1, FIRST, "rust", "fn a"),
        ]);
        let [done] = coloured.as_slice() else {
            panic!("one block coloured: {coloured:?}");
        };
        let expected = Highlighter::new(&other).code("rust", "fn a", &mut None);
        assert_eq!(debug(&done.text), debug(&expected));
    }

    /// A block streamed in, a piece at a time, each coloured before the next arrives, ends as
    /// it would coloured at once.
    #[test]
    fn a_streamed_block_ends_as_a_whole_one() {
        let code = "/* a\n   long comment */\nfn f() -> u8 {\n    1\n}\n";
        let (send, ordered) = std::sync::mpsc::channel();
        let (done, coloured) = std::sync::mpsc::channel();
        let mut last = None;
        std::thread::scope(|scope| {
            scope.spawn(move || {
                run(&mut Highlighter::new(&colours()), &ordered, &done, || true);
            });
            for end in (4..=code.len()).step_by(4).chain([code.len()]) {
                let piece = code.get(..end).expect("the code is ASCII");
                send.send(job(0, FIRST, "rust", piece))
                    .expect("the thread runs");
                last = Some(coloured.recv().expect("the thread colours it"));
            }
            drop(send);
        });
        let last = last.expect("pieces were sent");
        assert_eq!(last.job.raw, code);
        let whole = Highlighter::new(&colours()).code("rust", code, &mut None);
        assert_eq!(debug(&last.text), debug(&whole));
    }

    #[test]
    fn long_code_lets_the_window_be_longer() {
        assert_eq!(pause_after(0), MIN_WAKE);
        assert_eq!(pause_after(50), MIN_WAKE);
        assert_eq!(pause_after(1000), Duration::from_millis(210));
        assert!(pause_after(usize::MAX) >= Duration::from_secs(1));
    }

    #[test]
    fn the_window_is_woken_at_most_once_a_frame() {
        let (send, ordered) = std::sync::mpsc::channel();
        let (done, _coloured) = std::sync::mpsc::channel();
        let mut wakes = Vec::new();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                for line in 0..20 {
                    let code = "x = 1\n".repeat(line + 1);
                    send.send(job(0, FIRST, "py", &code))
                        .expect("the thread runs");
                    std::thread::sleep(Duration::from_millis(2));
                }
                drop(send);
            });
            run(&mut Highlighter::new(&colours()), &ordered, &done, || {
                wakes.push(Instant::now());
                true
            });
        });
        for pair in wakes.windows(2) {
            let [earlier, later] = pair else { continue };
            assert!(later.duration_since(*earlier) >= MIN_WAKE, "{wakes:?}");
        }
        assert!(wakes.len() < 20, "{} wakes", wakes.len());
    }
}
