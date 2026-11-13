//! The conversation's images, decoded for showing by the confined helper (see `decode`), which
//! the window starts itself: an image's bytes go to it from here, not through the daemon. One
//! is decoded at a time, on a thread of its own, so a conversation of many images doesn't run
//! many helpers at once. The Android app starts no programs of its own: it decodes on that
//! thread itself, in the sandbox Android keeps every app in, as a page does in its browser's.

use std::collections::HashSet;
use std::sync::mpsc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

use super::Event;
use crate::chat::{ImageBytes, ImageKey};
#[cfg(target_os = "android")]
use crate::decode::Refusal;
use crate::decode::{self, Pixels};

/// Why an image isn't shown.
#[derive(Debug, thiserror::Error)]
pub enum ShowError {
    #[error("its bytes aren't base64: {0}")]
    NotBase64(#[from] base64::DecodeError),
    #[error(transparent)]
    Decoder(#[from] decode::Error),
    #[cfg(target_os = "android")]
    #[error(transparent)]
    Refused(#[from] Refusal),
    /// Why is in the log, once.
    #[error("the window couldn't start decoding images")]
    NoRuntime,
}

/// The images asked for, and the thread that decodes them.
#[derive(Debug)]
pub(super) struct Images {
    jobs: mpsc::Sender<(ImageKey, ImageBytes)>,
    /// Those asked for and not dropped since: their pixels are taken when they come.
    asked: HashSet<ImageKey>,
}

impl Images {
    pub(super) fn start(events: async_channel::Sender<Event>) -> Self {
        let (jobs, taken) = mpsc::channel();
        std::thread::spawn(move || decode_all(&taken, &events));
        Self {
            jobs,
            asked: HashSet::new(),
        }
    }

    /// Asks for image `key` to be decoded, unless it was already; false if the thread is gone.
    pub(super) fn ask(&mut self, key: ImageKey, bytes: ImageBytes) -> bool {
        !self.asked.insert(key) || self.jobs.send((key, bytes)).is_ok()
    }

    /// Whether image `key` was asked for and not dropped since: what comes for one dropped is
    /// for a conversation no longer shown.
    pub(super) fn wanted(&self, key: ImageKey) -> bool {
        self.asked.contains(&key)
    }

    /// Drops the images but those in `shown`: the conversation was replaced.
    pub(super) fn keep(&mut self, shown: &HashSet<ImageKey>) {
        self.asked.retain(|key| shown.contains(key));
    }
}

/// Decodes each image asked for, in turn, until the window is gone.
fn decode_all(
    jobs: &mpsc::Receiver<(ImageKey, ImageBytes)>,
    events: &async_channel::Sender<Event>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => Some(runtime),
        Err(error) => {
            tracing::warn!(%error, "no images are shown: no runtime to decode them on");
            None
        }
    };
    for (key, bytes) in jobs {
        let shown = match &runtime {
            Some(runtime) => runtime.block_on(show(bytes)),
            None => Err(ShowError::NoRuntime),
        };
        if events
            .send_blocking(Event::ImageShown { key, shown })
            .is_err()
        {
            return;
        }
    }
}

async fn show(bytes: ImageBytes) -> Result<Pixels, ShowError> {
    let bytes = match bytes {
        ImageBytes::Base64(text) => BASE64.decode(text)?,
        ImageBytes::Sent(bytes) => bytes,
    };
    #[cfg(target_os = "android")]
    return Ok(decode::shown(&bytes)?);
    #[cfg(not(target_os = "android"))]
    Ok(decode::show(&bytes).await?)
}
