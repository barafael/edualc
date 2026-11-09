//! The window's line to its daemon, in the browser demo: the daemon is a script in the same
//! page (`web::daemon`), and the line a channel to it. What it answers comes to the app as the
//! same events the Linux line makes of the daemon's messages.

use async_channel::TrySendError;

use crate::ipc::{self, Generation, ToDaemon};

pub(crate) type Link = async_channel::Sender<ToDaemon>;

#[derive(Debug)]
pub(crate) struct Lost;

pub(crate) fn send(link: &Link, message: ToDaemon) -> Result<(), Lost> {
    match link.try_send(message) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_) | TrySendError::Closed(_)) => Err(Lost),
    }
}

/// A line for a session's input, in pieces that each fit a message, as on Linux.
pub(crate) fn send_input(link: &Link, key: Generation, line: &str) -> Result<(), Lost> {
    let mut pieces = ipc::chunks(line).peekable();
    while let Some(text) = pieces.next() {
        let last = pieces.peek().is_none();
        send(
            link,
            ToDaemon::Input {
                key,
                text: text.to_owned(),
                last,
            },
        )?;
    }
    Ok(())
}
