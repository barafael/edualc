//! A window's line to its daemon, whatever carries it (the native window's Unix socket, a
//! page's WebSocket): what the window asks goes on a channel to whatever writes it, and what
//! the daemon says comes back as the app's events (`Inbox`).

use std::collections::HashMap;

use async_channel::{Sender, TrySendError};

use crate::app::Event;
use crate::claude::Output;
use crate::ipc::{ActId, FromDaemon, Generation, Hosted, PROTOCOL, Stream, ToDaemon};
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
    /// Claude Code's own sandbox is on in its settings.
    pub claude_codes_sandbox: bool,
    /// The user's home folder.
    pub home: Option<std::path::PathBuf>,
}

/// Why a daemon's first messages don't greet this window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Ungreeted {
    #[error("edualcd runs another version of edualc: update the older of the two")]
    Version,
    #[error("edualcd said something out of turn as the window connected")]
    OutOfTurn,
}

/// What a daemon's first two messages, its hello and the sessions it runs, tell a window.
pub fn greeted(hello: FromDaemon, hosted: FromDaemon) -> Result<Greeting, Ungreeted> {
    let FromDaemon::Hello {
        protocol,
        settings,
        live,
        claude_codes_sandbox,
        home,
    } = hello
    else {
        return Err(Ungreeted::Version);
    };
    if protocol != PROTOCOL {
        return Err(Ungreeted::Version);
    }
    let FromDaemon::Hosted(hosted) = hosted else {
        return Err(Ungreeted::OutOfTurn);
    };
    let greeting = Greeting {
        hosted,
        live,
        settings,
        claude_codes_sandbox,
        home,
    };
    Ok(greeting)
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
