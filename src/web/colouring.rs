//! The browser demo's version of `colouring`: colours each block when asked, on the page's only
//! thread.

use crate::app::Event;
use crate::generated::SyntaxColours;
use crate::highlight::{Colourer, Job};

pub(crate) struct Colouring {
    colourer: Colourer,
    events: async_channel::Sender<Event>,
}

impl Colouring {
    pub(crate) fn start(colours: &SyntaxColours, events: async_channel::Sender<Event>) -> Self {
        Self {
            colourer: Colourer::new(colours),
            events,
        }
    }

    pub(crate) fn ask(&mut self, job: Job) {
        let coloured = self.colourer.colour(job);
        if self.events.try_send(Event::CodeColoured(coloured)).is_err() {
            tracing::debug!("the window's events are gone; code stays plain");
        }
    }

    pub(crate) fn set_colours(&mut self, colours: &SyntaxColours) {
        self.colourer.set_colours(colours);
    }

    pub(crate) fn forget(&mut self) {
        self.colourer.forget();
    }
}
