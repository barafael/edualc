//! A window's line to its daemon, whatever carries it (the native window's Unix socket, a
//! page's WebSocket): what the window asks goes on a channel to whatever writes it, and what
//! the daemon says comes back as the app's events (`Inbox`).

use std::collections::HashMap;

use async_channel::{Sender, TrySendError};

use crate::app::Event;
use crate::claude::Output;
use crate::desktop::Opener;
use crate::ipc::{ActId, FromDaemon, Generation, Hosted, Stream, ToDaemon};
use crate::sessions::{self, LiveSession};
use crate::settings::Settings;

/// What the window asks of the daemon goes this way, in order. Bounded: a daemon that stopped
/// taking messages loses the line rather than the window its memory.
pub type Link = Sender<ToDaemon>;

/// Messages waiting to be written to the daemon.
pub const QUEUE: usize = 1024;

/// The line was lost: the daemon ended, or stopped taking messages.
#[derive(Debug)]
pub struct Lost;

/// Sends a message the window doesn't wait on.
pub fn send(link: &Link, message: ToDaemon) -> Result<(), Lost> {
    match link.try_send(message) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_) | TrySendError::Closed(_)) => Err(Lost),
    }
}

/// A line for a session's input.
pub fn send_input(link: &Link, key: Generation, line: &str) -> Result<(), Lost> {
    let line = line.to_owned();
    send(link, ToDaemon::Input { key, line })
}

/// What the daemon tells a window as it connects.
#[derive(Debug)]
pub struct Greeting {
    /// The sessions the daemon runs: known before the window opens one, so that one it runs
    /// is attached to rather than started a second time.
    pub hosted: Vec<Hosted>,
    /// The sessions running now (in terminals, say): one of them resumed is watched instead.
    pub live: Vec<LiveSession>,
    /// The settings, or why they can't be read.
    pub settings: Result<Settings, String>,
    /// What folders open in, besides the window.
    pub openers: Vec<Opener>,
    /// Claude Code's own sandbox is on in its settings.
    pub claude_codes_sandbox: bool,
    /// Other programs' windows can be found.
    pub finds_windows: bool,
}

/// What the daemon says, made the app's events (sans-io): a transcript's lines are kept until
/// the answer they belong to comes, and read then, off the app's thread.
#[derive(Debug, Default)]
pub struct Inbox {
    lines: HashMap<ActId, Vec<String>>,
}

impl Inbox {
    pub fn take(&mut self, message: FromDaemon) -> Option<Event> {
        Some(match message {
            FromDaemon::Started {
                key,
                pid,
                enforcement,
                turns,
            } => claude(
                key,
                Output::Started {
                    pid,
                    enforcement,
                    turns,
                },
            ),
            FromDaemon::Failed { key, failure } => claude(key, Output::Failed(failure)),
            FromDaemon::Output { key, stream, line } => claude(
                key,
                match stream {
                    Stream::Out => Output::of_line(line),
                    Stream::Err => Output::Log(line),
                },
            ),
            FromDaemon::Ended { key, code } => claude(key, Output::Ended(code)),
            FromDaemon::Diverged { key, left } => claude(key, Output::Diverged(left)),
            FromDaemon::Done { request, result } => Event::Acted { request, result },
            FromDaemon::Picked { request, folder } => Event::Picked { request, folder },
            FromDaemon::Trusted {
                request,
                accepted,
                runs,
            } => Event::Trusted {
                request,
                accepted,
                runs,
            },
            FromDaemon::Hosted(sessions) => Event::Hosted(sessions),
            FromDaemon::Pasted { request, pasted } => Event::Pasted { request, pasted },
            FromDaemon::Lines { request, lines } => {
                self.lines.entry(request).or_default().extend(lines);
                return None;
            }
            FromDaemon::Answer { request, answer } => {
                let lines = self.lines.remove(&request).unwrap_or_default();
                Event::Answered {
                    request,
                    answer,
                    lines: sessions::parse_lines(&lines),
                }
            }
            FromDaemon::Changed(watched) => Event::Changed(watched),
            FromDaemon::Hello { .. } => return None,
        })
    }
}

fn claude(generation: Generation, output: Output) -> Event {
    Event::Claude { generation, output }
}
