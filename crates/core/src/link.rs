//! A window's line to its daemon, whatever carries it (the native window's Unix socket, a
//! network line, the browser demo's script): what the window asks goes on a channel to whatever
//! writes it, and what the daemon says comes back as the app's events (`Inbox`, sans-io).

#[cfg(not(target_arch = "wasm32"))]
use std::collections::{BTreeMap, HashMap};

use crate::app::Event;
use crate::claude::Output;
#[cfg(not(target_arch = "wasm32"))]
use crate::decode;
#[cfg(not(target_arch = "wasm32"))]
use crate::desktop::Opener;
use crate::ipc::{self, Generation, ToDaemon};
#[cfg(not(target_arch = "wasm32"))]
use crate::ipc::{
    ActId, Assembler, FromDaemon, Hosted, LineSplitter, PasteFailure, Pasted, Stream,
};
#[cfg(not(target_arch = "wasm32"))]
use crate::sessions::{self, LiveSession};
#[cfg(not(target_arch = "wasm32"))]
use crate::settings::Settings;

/// What the window asks of the daemon goes this way, in order. Bounded: a daemon that stopped
/// taking messages loses the line rather than the window its memory.
#[cfg(not(target_arch = "wasm32"))]
pub type Link = std::sync::mpsc::SyncSender<ToDaemon>;
/// In the browser demo, the line is a channel to the script in the same page.
#[cfg(target_arch = "wasm32")]
pub type Link = async_channel::Sender<ToDaemon>;

/// Messages waiting to be written to the daemon.
pub const QUEUE: usize = 1024;

/// The line was lost: the daemon ended, or stopped taking messages.
#[derive(Debug)]
pub struct Lost;

/// Sends a message the window doesn't wait on.
pub fn send(link: &Link, message: ToDaemon) -> Result<(), Lost> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::sync::mpsc::TrySendError;
        match link.try_send(message) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => Err(Lost),
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        use async_channel::TrySendError;
        match link.try_send(message) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_) | TrySendError::Closed(_)) => Err(Lost),
        }
    }
}

/// A line for a session's input, in pieces that each fit a message.
pub fn send_input(link: &Link, key: Generation, line: &str) -> Result<(), Lost> {
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

/// What the daemon tells a window as it connects.
#[cfg(not(target_arch = "wasm32"))]
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
    /// Other programs' windows can be found.
    pub finds_windows: bool,
}

/// What the daemon says, made the app's events (sans-io): the pieces a line, a pasted image or a
/// transcript comes in are put together again, and an event made when one is whole.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default)]
pub struct Inbox {
    lines: Assembler<(Generation, Stream)>,
    pasting: BTreeMap<ActId, Joining>,
    /// A transcript's text, made lines as it comes, kept for the answer it belongs to.
    transcripts: HashMap<ActId, (LineSplitter, Vec<String>)>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Inbox {
    /// What `message` makes of the app's events, if it makes one (a piece of a line doesn't).
    pub fn take(&mut self, message: FromDaemon) -> Option<Event> {
        match message {
            FromDaemon::Started { key, pid, turns } => {
                Some(claude(key, Output::Started { pid, turns }))
            }
            FromDaemon::Failed { key, failure } => Some(claude(key, Output::Failed(failure))),
            FromDaemon::Output {
                key,
                stream,
                text,
                last,
            } => match self.lines.push((key, stream), &text, last) {
                Ok(Some(line)) => Some(claude(
                    key,
                    match stream {
                        Stream::Out => Output::of_line(line),
                        Stream::Err => Output::Log(line),
                    },
                )),
                Ok(None) => None,
                Err(error) => {
                    tracing::warn!(%error, "a line from claude too long to keep");
                    None
                }
            },
            FromDaemon::Ended { key, code } => {
                self.lines.forget(&(key, Stream::Out));
                self.lines.forget(&(key, Stream::Err));
                Some(claude(key, Output::Ended(code)))
            }
            FromDaemon::Done { request, result } => Some(Event::Acted { request, result }),
            FromDaemon::Picked { request, folder } => Some(Event::Picked { request, folder }),
            FromDaemon::Trusted {
                request,
                accepted,
                runs,
            } => Some(Event::Trusted {
                request,
                accepted,
                runs,
            }),
            FromDaemon::Hosted(sessions) => Some(Event::Hosted(sessions)),
            FromDaemon::Pasted {
                request,
                piece,
                last,
            } => joined(&mut self.pasting, request, &piece, last)
                .map(|pasted| Event::Pasted { request, pasted }),
            FromDaemon::ShuttingDown => Some(Event::DaemonShuttingDown),
            FromDaemon::Lines { request, text } => {
                let (splitter, lines) = self.transcripts.entry(request).or_default();
                lines.extend(splitter.push(text.as_bytes()));
                None
            }
            FromDaemon::Answer { request, answer } => {
                let (_unfinished, lines) = self.transcripts.remove(&request).unwrap_or_default();
                Some(Event::Answered {
                    request,
                    answer,
                    lines: sessions::parse_lines(&lines),
                })
            }
            FromDaemon::Changed(watched) => Some(Event::Changed(watched)),
            FromDaemon::Hello { .. } => None,
        }
    }
}

fn claude(generation: Generation, output: Output) -> Event {
    Event::Claude { generation, output }
}

/// A pasted image's pieces, as they come.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
enum Joining {
    Bytes(Vec<u8>),
    /// More came than an image comes to: told once, the rest dropped.
    Overflowed,
}

/// A pasted image, once its last piece came (sans-io); `None` while pieces come, and for the
/// rest of one that overflowed.
#[cfg(not(target_arch = "wasm32"))]
fn joined(
    pasting: &mut BTreeMap<ActId, Joining>,
    request: ActId,
    piece: &[u8],
    last: bool,
) -> Option<Pasted> {
    let joining = pasting
        .entry(request)
        .or_insert_with(|| Joining::Bytes(Vec::new()));
    let told = match joining {
        Joining::Bytes(bytes) if bytes.len() + piece.len() > decode::MAX_ANSWER_BYTES => {
            *joining = Joining::Overflowed;
            Some(Err(PasteFailure::Decoder(
                "the image came in more pieces than an image comes to".to_owned(),
            )))
        }
        Joining::Bytes(bytes) => {
            bytes.extend_from_slice(piece);
            None
        }
        Joining::Overflowed => None,
    };
    if !last {
        return told;
    }
    match pasting.remove(&request) {
        Some(Joining::Bytes(bytes)) => Some(
            postcard::from_bytes::<Pasted>(&bytes)
                .unwrap_or_else(|error| Err(PasteFailure::Decoder(error.to_string()))),
        ),
        Some(Joining::Overflowed) | None => told,
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::decode::{Picture, Pixels, Refusal};
    use crate::external::wire::MediaType;

    /// What the daemon may tell of a paste: an image, or why there is none. The kind is picked
    /// by number: `prop_oneof!` holds its choices in `Arc`s.
    fn any_pasted() -> impl Strategy<Value = Pasted> {
        (
            0u8..=10,
            ".{0,200}",
            proptest::collection::vec(any::<u8>(), 0..2048),
            any::<(u32, u32)>(),
            proptest::collection::vec(any::<u8>(), 0..512),
        )
            .prop_map(|(kind, text, bytes, (width_px, height_px), rgba)| {
                let picture = |media_type| Picture {
                    media_type,
                    bytes,
                    width_px,
                    height_px,
                    thumbnail: Pixels {
                        width_px: width_px % decode::THUMBNAIL_SIDE_PX,
                        height_px: height_px % decode::THUMBNAIL_SIDE_PX,
                        rgba,
                    },
                };
                match kind {
                    0 => Ok(picture(MediaType::Png)),
                    1 => Ok(picture(MediaType::Jpeg)),
                    2 => Err(PasteFailure::NoImage),
                    3 => Err(PasteFailure::Clipboard(text)),
                    4 => Err(PasteFailure::Decoder(text)),
                    5 => Err(PasteFailure::Refused(Refusal::TooManyBytes)),
                    6 => Err(PasteFailure::Refused(Refusal::UnknownFormat)),
                    7 => Err(PasteFailure::Refused(Refusal::TooLarge)),
                    8 => Err(PasteFailure::Refused(Refusal::Undecodable(text))),
                    9 => Err(PasteFailure::Refused(Refusal::Unencodable(text))),
                    _ => Err(PasteFailure::Refused(Refusal::TooHeavy)),
                }
            })
    }

    /// Cuts `bytes` into pieces at `cuts` (offsets, in any order); one empty piece for none.
    fn pieces<'bytes>(bytes: &'bytes [u8], cuts: &[usize]) -> Vec<&'bytes [u8]> {
        let mut offsets: Vec<usize> = cuts.iter().map(|cut| cut % (bytes.len() + 1)).collect();
        offsets.extend([0, bytes.len()]);
        offsets.sort_unstable();
        offsets.dedup();
        let cut: Vec<&[u8]> = offsets
            .windows(2)
            .filter_map(|pair| bytes.get(pair.first().copied()?..pair.last().copied()?))
            .collect();
        if cut.is_empty() { vec![&[]] } else { cut }
    }

    proptest! {
        /// Two pastes' pieces, however cut and interleaved, each come together into what the
        /// daemon sent, told once, as its last piece comes.
        #[test]
        fn interleaved_pastes_join_into_what_was_sent(
            first_sent in any_pasted(),
            second_sent in any_pasted(),
            first_cuts in proptest::collection::vec(any::<usize>(), 0..12),
            second_cuts in proptest::collection::vec(any::<usize>(), 0..12),
            order in proptest::collection::vec(any::<bool>(), 0..30),
        ) {
            let first_request = ActId::default().next();
            let second_request = first_request.next();
            let first_bytes = postcard::to_stdvec(&first_sent).expect("encodes");
            let second_bytes = postcard::to_stdvec(&second_sent).expect("encodes");
            let mut first = pieces(&first_bytes, &first_cuts).into_iter().peekable();
            let mut second = pieces(&second_bytes, &second_cuts).into_iter().peekable();
            let mut pasting = BTreeMap::new();
            let mut told = Vec::new();
            let mut turns = order.into_iter().chain(std::iter::repeat(true));
            while first.peek().is_some() || second.peek().is_some() {
                let take_first = second.peek().is_none()
                    || (first.peek().is_some() && turns.next().unwrap_or(true));
                let (request, piece, rest) = if take_first {
                    (first_request, first.next(), &mut first)
                } else {
                    (second_request, second.next(), &mut second)
                };
                let piece = piece.expect("a piece left");
                let last = rest.peek().is_none();
                let answer = joined(&mut pasting, request, piece, last);
                prop_assert!(last || answer.is_none(), "told before the last piece");
                told.extend(answer.map(|pasted| (request, pasted)));
            }
            prop_assert!(pasting.is_empty());
            let mut expected = vec![(first_request, first_sent), (second_request, second_sent)];
            expected.sort_by_key(|(request, _)| *request);
            told.sort_by_key(|(request, _)| *request);
            prop_assert_eq!(told, expected);
        }

    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        /// Whatever pieces come, a paste is told of exactly once: when its pieces first come to
        /// more than an image may, or else at its last piece. Never a panic, nothing kept after.
        #[test]
        fn any_pieces_are_told_of_once(
            sizes in proptest::collection::vec(0..=decode::MAX_ANSWER_BYTES / 3, 1..8),
            filler in any::<u8>(),
        ) {
            let request = ActId::default().next();
            let mut pasting = BTreeMap::new();
            let mut told = 0;
            let mut so_far = 0;
            let count = sizes.len();
            for (position, size) in sizes.into_iter().enumerate() {
                let was_over = so_far > decode::MAX_ANSWER_BYTES;
                so_far += size;
                let last = position + 1 == count;
                let answer = joined(&mut pasting, request, &vec![filler; size], last);
                if answer.is_some() {
                    told += 1;
                    prop_assert!(
                        last || (so_far > decode::MAX_ANSWER_BYTES && !was_over),
                        "told at piece {position} of {count}, {so_far} bytes in"
                    );
                }
            }
            prop_assert_eq!(told, 1);
            prop_assert!(pasting.is_empty());
        }
    }

    /// A pasted image's pieces come together in order; too many are told of once and the
    /// rest dropped; bytes that don't parse are an error, not a panic.
    #[test]
    fn pasted_pieces_join_once_and_no_further() {
        let mut pasting = BTreeMap::new();
        let request = ActId::default().next();
        let sent: Pasted = Err(PasteFailure::NoImage);
        let bytes = postcard::to_stdvec(&sent).expect("encodes");
        let (first, rest) = bytes.split_at(1);
        assert_eq!(joined(&mut pasting, request, first, false), None);
        assert_eq!(joined(&mut pasting, request, rest, true), Some(sent));
        assert!(pasting.is_empty());

        let huge = vec![0u8; decode::MAX_ANSWER_BYTES + 1];
        assert!(matches!(
            joined(&mut pasting, request, &huge, false),
            Some(Err(PasteFailure::Decoder(_)))
        ));
        assert_eq!(joined(&mut pasting, request, b"more", false), None);
        assert_eq!(
            joined(&mut pasting, request, b"end", true),
            None,
            "told once"
        );
        assert!(pasting.is_empty());

        assert!(matches!(
            joined(&mut pasting, request, &[0xff, 0xff], true),
            Some(Err(PasteFailure::Decoder(_)))
        ));
    }
}
