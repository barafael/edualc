//! Noticing files change, in the browser demo: its files (the demo's sessions) are built into
//! the page and never change, so a watch is taken and nothing is ever noticed.

use std::path::Path;

use async_channel::Sender;

use crate::app::Event;

/// A file or folder watched; all are alike, as none is ever noticed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Watched;

/// Watches that never fire.
#[derive(Debug)]
pub(crate) struct Watcher;

/// Nothing goes wrong watching nothing; Linux's errors have no counterpart here.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {}

impl Watcher {
    pub(crate) fn start(_events: Sender<Event>) -> Result<Self, Error> {
        Ok(Self)
    }

    pub(crate) fn folder(&self, _dir: &Path) -> Result<Watched, Error> {
        Ok(Watched)
    }

    pub(crate) fn file(&self, _file: &Path) -> Result<Watched, Error> {
        Ok(Watched)
    }

    pub(crate) fn forget(&self, _watched: Watched) {}
}
