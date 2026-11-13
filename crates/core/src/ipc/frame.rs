//! What a line's messages are made into and taken from: postcard's COBS frames, each ended by a
//! `0`, and the errors a line has. Wherever there is a line: the window's socket, edualcd's
//! network lines, and a page, which is built for `wasm32`.

use std::collections::VecDeque;

use serde::{Serialize, de::DeserializeOwned};

use super::MAX_MESSAGE;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[cfg(not(target_arch = "wasm32"))]
    #[error("XDG_RUNTIME_DIR isn't set: there is no private place for the daemon's socket")]
    NoRuntimeDir,
    #[cfg(not(target_arch = "wasm32"))]
    #[error("{0} isn't the user's alone (it must be a folder they own, closed to others)")]
    NotPrivate(std::path::PathBuf),
    #[cfg(not(target_arch = "wasm32"))]
    #[error("couldn't open {path}: {source}")]
    Open {
        path: std::path::PathBuf,
        #[source]
        source: rustix::io::Errno,
    },
    #[cfg(not(target_arch = "wasm32"))]
    #[error("the daemon's socket: {0}")]
    Socket(#[source] rustix::io::Errno),
    #[cfg(not(target_arch = "wasm32"))]
    #[error("the daemon's socket: {0}")]
    Io(#[source] std::io::Error),
    #[error("a message bigger than {MAX_MESSAGE} bytes")]
    Oversized,
    /// postcard's accumulator says no more than that.
    #[error("a message that doesn't parse")]
    Unreadable,
    #[error("couldn't encode a message: {0}")]
    Encode(#[source] postcard::Error),
    #[error("the other side hung up")]
    Closed,
    #[cfg(not(target_arch = "wasm32"))]
    #[error("a peer that isn't this user's edualc (uid {uid}, pid {pid})")]
    Stranger { uid: u32, pid: i32 },
}

/// The largest frame: a message COBS-encoded, at most one byte more per 254, and its `0`.
pub const MAX_FRAME: usize = MAX_MESSAGE + MAX_MESSAGE / 254 + 2;

/// A connection's bytes, as they come, made into messages (sans-io: the window's blocking reads
/// and the daemon's tokio reads feed the same one). Frames are postcard's COBS, each ended by a
/// `0`. The frame being read is kept on the heap, and never grows past [`MAX_FRAME`]: a longer
/// frame, or one that doesn't parse, ends the connection (a peer here sending one is broken or
/// hostile; the next frame isn't gone on to, as a lossy serial line would).
pub struct Deframer<Message> {
    /// The bytes of the frame so far, up to its `0`.
    frame: Vec<u8>,
    /// Whole messages a read brought, not yet taken.
    ready: VecDeque<Message>,
}

impl<Message> Default for Deframer<Message> {
    fn default() -> Self {
        Self {
            frame: Vec::new(),
            ready: VecDeque::new(),
        }
    }
}

impl<Message> std::fmt::Debug for Deframer<Message> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Deframer")
            .field("ready", &self.ready.len())
            .finish_non_exhaustive()
    }
}

impl<Message: DeserializeOwned> Deframer<Message> {
    /// Bytes read; an empty read is the other side hanging up.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if bytes.is_empty() {
            return Err(Error::Closed);
        }
        let mut rest = bytes;
        while let Some(end) = rest.iter().position(|&byte| byte == 0) {
            let (last, after) = rest.split_at(end);
            self.add(last)?;
            let message = postcard::from_bytes_cobs::<Message>(&mut self.frame)
                .map_err(|_unreadable| Error::Unreadable)?;
            self.frame.clear();
            self.ready.push_back(message);
            rest = after.get(1..).unwrap_or_default();
        }
        self.add(rest)
    }

    /// Adds to the frame being read, which may not outgrow [`MAX_FRAME`].
    fn add(&mut self, bytes: &[u8]) -> Result<(), Error> {
        if self.frame.len() + bytes.len() > MAX_FRAME {
            return Err(Error::Oversized);
        }
        self.frame.extend_from_slice(bytes);
        Ok(())
    }

    /// The next whole message, if one came.
    pub fn take(&mut self) -> Option<Message> {
        self.ready.pop_front()
    }
}

/// The message in one frame (as [`frame`] makes it, its `0` left off or on): for a frame that
/// came whole, the first of a line.
pub fn deframe<T: DeserializeOwned>(frame: &[u8]) -> Result<T, Error> {
    let mut bytes = frame.strip_suffix(&[0]).unwrap_or(frame).to_vec();
    postcard::from_bytes_cobs::<T>(&mut bytes).map_err(|_unreadable| Error::Unreadable)
}

/// A message as a frame: postcard's COBS, ended by a `0`.
pub fn frame(message: &impl Serialize) -> Result<Vec<u8>, Error> {
    let frame = postcard::to_stdvec_cobs(message).map_err(Error::Encode)?;
    if frame.len() > MAX_FRAME {
        return Err(Error::Oversized);
    }
    Ok(frame)
}
