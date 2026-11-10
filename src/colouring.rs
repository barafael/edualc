//! The conversation's code, coloured on a thread of its own (see `highlight`), so opening a
//! long session or switching the look doesn't hold up the window. What it colours comes back
//! as [`Event::CodeColoured`]. Of several asks for one block waiting together, only the last is
//! coloured: a block streaming in faster than it is coloured skips what it has outgrown.

use std::collections::HashMap;
use std::sync::mpsc;

use crate::app::Event;
use crate::generated::SyntaxColours;
use crate::highlight::{BlockAt, Colourer, Job};

enum Order {
    Colour(Job),
    Colours(SyntaxColours),
    Forget,
}

/// The colouring thread, reached by orders; it ends with the window.
pub(crate) struct Colouring {
    orders: mpsc::Sender<Order>,
}

impl Colouring {
    pub(crate) fn start(colours: &SyntaxColours, events: async_channel::Sender<Event>) -> Self {
        let (orders, taken) = mpsc::channel();
        let colours = colours.clone();
        std::thread::spawn(move || colour_all(&taken, &colours, &events));
        Self { orders }
    }

    pub(crate) fn ask(&self, job: Job) {
        self.send(Order::Colour(job));
    }

    /// Other colours, for what is asked from now on.
    pub(crate) fn set_colours(&self, colours: &SyntaxColours) {
        self.send(Order::Colours(colours.clone()));
    }

    /// The view dropped its blocks.
    pub(crate) fn forget(&self) {
        self.send(Order::Forget);
    }

    fn send(&self, order: Order) {
        if self.orders.send(order).is_err() {
            tracing::debug!("the colouring thread is gone; code stays plain");
        }
    }
}

fn colour_all(
    orders: &mpsc::Receiver<Order>,
    colours: &SyntaxColours,
    events: &async_channel::Sender<Event>,
) {
    let mut colourer = Colourer::new(colours);
    while let Ok(first) = orders.recv() {
        let waiting: Vec<Order> = std::iter::once(first).chain(orders.try_iter()).collect();
        let last_ask: HashMap<BlockAt, usize> = waiting
            .iter()
            .enumerate()
            .filter_map(|(position, order)| match order {
                Order::Colour(job) => Some((job.at, position)),
                Order::Colours(_) | Order::Forget => None,
            })
            .collect();
        for (position, order) in waiting.into_iter().enumerate() {
            match order {
                Order::Colour(job) if last_ask.get(&job.at) == Some(&position) => {
                    let coloured = colourer.colour(job);
                    if events.send_blocking(Event::CodeColoured(coloured)).is_err() {
                        return;
                    }
                }
                Order::Colour(_) => {}
                Order::Colours(colours) => colourer.set_colours(&colours),
                Order::Forget => colourer.forget(),
            }
        }
    }
}
