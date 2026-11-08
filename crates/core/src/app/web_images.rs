//! The conversation's images, in the browser demo: decoded in the page as they are asked for.
//! The page is the sandbox the confined helper is on Linux (see `decode`), and has no threads
//! to decode on beside it.

use std::collections::HashSet;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use super::Event;
use crate::chat::{ImageBytes, ImageKey};
use crate::decode::{self, Pixels, Refusal};

/// Why an image isn't shown.
#[derive(Debug, thiserror::Error)]
pub enum ShowError {
    #[error("its bytes aren't base64: {0}")]
    NotBase64(#[from] base64::DecodeError),
    #[error(transparent)]
    Refused(#[from] Refusal),
}

/// The images asked for; each is decoded as it is asked for.
#[derive(Debug)]
pub(super) struct Images {
    events: async_channel::Sender<Event>,
    /// Those asked for and not dropped since: their pixels are taken when they come.
    asked: HashSet<ImageKey>,
}

impl Images {
    pub(super) fn start(events: async_channel::Sender<Event>) -> Self {
        Self {
            events,
            asked: HashSet::new(),
        }
    }

    /// Decodes image `key`, unless it was already; false if the window's events are gone.
    pub(super) fn ask(&mut self, key: ImageKey, bytes: ImageBytes) -> bool {
        if !self.asked.insert(key) {
            return true;
        }
        let shown = show(bytes);
        self.events
            .try_send(Event::ImageShown { key, shown })
            .is_ok()
    }

    /// Whether image `key` was asked for and not dropped since.
    pub(super) fn wanted(&self, key: ImageKey) -> bool {
        self.asked.contains(&key)
    }

    /// Drops the images but those in `shown`: the conversation was replaced.
    pub(super) fn keep(&mut self, shown: &HashSet<ImageKey>) {
        self.asked.retain(|key| shown.contains(key));
    }
}

fn show(bytes: ImageBytes) -> Result<Pixels, ShowError> {
    let bytes = match bytes {
        ImageBytes::Base64(text) => BASE64.decode(text)?,
        ImageBytes::Sent(bytes) => bytes,
    };
    Ok(decode::shown(&bytes)?)
}
