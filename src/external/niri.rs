//! niri's IPC on `$NIRI_SOCKET`, as `niri msg` speaks it: a request as one JSON line, its reply
//! as another, `{"Ok": …}` or `{"Err": "why"}`. Only the window list and focusing a window, typed
//! with just the fields read: niri's own types (the `niri-ipc` crate) require fields that older
//! releases don't send (`layout` came in 25.08), and are pinned to one release.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How long niri gets to take a request or answer it; a compositor that doesn't answer in this
/// time isn't waited for.
const PATIENCE: Duration = Duration::from_secs(2);
/// The longest reply read: a window list of thousands of windows fits.
const MAX_REPLY_BYTES: u64 = 4 * 1024 * 1024;

/// niri's id for a window, valid while it is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WindowId(u64);

/// A window as niri lists it, in the fields read.
#[derive(Debug, Deserialize)]
pub(crate) struct Window {
    pub(crate) id: WindowId,
    /// The process that made the window's Wayland connection, if niri knows it.
    #[serde(default)]
    pub(crate) pid: Option<i32>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) enum Request {
    Windows,
    Action(Action),
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub(crate) enum Action {
    FocusWindow { id: WindowId },
}

#[derive(Debug, Deserialize)]
pub(crate) enum Response {
    Windows(Vec<Window>),
    Handled,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("niri isn't running here (NIRI_SOCKET is unset)")]
    NotRunning,
    #[error("couldn't talk to niri: {0}")]
    Socket(#[source] std::io::Error),
    #[error("couldn't write the request for niri: {0}")]
    Request(#[source] serde_json::Error),
    #[error("niri's answer doesn't parse: {0}")]
    Answer(#[source] serde_json::Error),
    #[error("niri refused: {0}")]
    Refused(String),
}

/// Asks niri one thing, on a connection of its own.
pub(crate) fn ask(request: &Request) -> Result<Response, Error> {
    let socket = std::env::var_os("NIRI_SOCKET").ok_or(Error::NotRunning)?;
    let mut stream = UnixStream::connect(socket).map_err(Error::Socket)?;
    stream
        .set_read_timeout(Some(PATIENCE))
        .and_then(|()| stream.set_write_timeout(Some(PATIENCE)))
        .map_err(Error::Socket)?;
    let mut line = serde_json::to_vec(request).map_err(Error::Request)?;
    line.push(b'\n');
    stream.write_all(&line).map_err(Error::Socket)?;
    let mut reply = String::new();
    BufReader::new(stream.take(MAX_REPLY_BYTES))
        .read_line(&mut reply)
        .map_err(Error::Socket)?;
    parse_reply(&reply)
}

fn parse_reply(reply: &str) -> Result<Response, Error> {
    match serde_json::from_str::<Result<Response, String>>(reply).map_err(Error::Answer)? {
        Ok(response) => Ok(response),
        Err(why) => Err(Error::Refused(why)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requests as niri reads them: `niri-ipc` 26.4.0 declares `Request::Windows`,
    /// `Request::Action(Action)` and `Action::FocusWindow { id }`, externally tagged.
    #[test]
    fn requests_are_written_as_niri_reads_them() {
        assert_eq!(
            serde_json::to_string(&Request::Windows).ok().as_deref(),
            Some(r#""Windows""#)
        );
        let focus = Request::Action(Action::FocusWindow { id: WindowId(13) });
        assert_eq!(
            serde_json::to_string(&focus).ok().as_deref(),
            Some(r#"{"Action":{"FocusWindow":{"id":13}}}"#)
        );
    }

    /// Window lists of three niri releases parse alike: 25.02 (before `is_urgent` and
    /// `layout`), and 26.04 as the running niri sent it (2026-11-01, title and app id replaced),
    /// with a field no release has yet.
    #[test]
    fn window_lists_of_older_and_newer_niri_parse() {
        let releases = [
            r#"{"Ok":{"Windows":[{"id":4,"title":"t","app_id":"a","pid":18756,"workspace_id":1,"is_focused":true,"is_floating":false}]}}"#,
            r#"{"Ok":{"Windows":[{"id":4,"title":"<title>","app_id":"<app id>","pid":18756,"workspace_id":7,"is_focused":false,"is_floating":false,"is_urgent":false,"layout":{"pos_in_scrolling_layout":[1,1],"tile_size":[952.0,1269.3333333333333],"window_size":[952,1269],"tile_pos_in_workspace_view":null,"window_offset_in_tile":[0.0,0.0]},"focus_timestamp":{"secs":140761,"nanos":586468694}}]}}"#,
            r#"{"Ok":{"Windows":[{"id":4,"pid":null,"is_focused":false,"a_field_from_later":[]}]}}"#,
        ];
        for reply in releases {
            match parse_reply(reply) {
                Ok(Response::Windows(windows)) => {
                    assert_eq!(windows.first().map(|window| window.id), Some(WindowId(4)));
                }
                other => panic!("{reply} came to {other:?}"),
            }
        }
    }

    /// What niri says to a request it doesn't take.
    #[test]
    fn a_refusal_keeps_niris_reason() {
        assert!(matches!(
            parse_reply("{\"Err\":\"error parsing request\"}\n"),
            Err(Error::Refused(why)) if why == "error parsing request"
        ));
        assert!(matches!(
            parse_reply(r#"{"Ok":"Handled"}"#),
            Ok(Response::Handled)
        ));
    }
}
