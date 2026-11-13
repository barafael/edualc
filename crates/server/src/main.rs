//! `edualcd`: edualc's daemon, and the server that makes edualc's window a page.
//!
//! - It runs Claude Code (and agy) for edualc's windows, reads its files for them and does
//!   what they ask (`edualc-host`): for windows on this machine by the Unix socket, and, with
//!   `--listen`, for windows that aren't (a page, an app) over TLS, each at `/api/line` with the
//!   token edualcd keeps. It runs as a user service (`edualcd.service`) and keeps its sessions
//!   while windows come and go.
//! - With `--listen` it is also a Dioxus fullstack server: it serves the page (`web`), the same
//!   window as the native one, drawn by Dioxus from the same presenter, whose line is a
//!   window's like any other (`line`).
//! - `edualcd --decode-image` and `--show-image` are the confined image decoder (see `decode`).
//!
//! Built with the `mobile` feature instead (`dx build --android`), it is the Android app
//! (`android`): the same window, which reaches an edualcd over the network, natively.

#[cfg(feature = "server")]
mod serve;

// dx builds the server for the app with the app's features too: there nothing runs it.
#[cfg(feature = "mobile")]
#[cfg_attr(feature = "server", allow(dead_code))]
mod android;
mod line;
mod web;

#[cfg(feature = "server")]
fn main() -> std::process::ExitCode {
    serve::main()
}

#[cfg(not(any(feature = "server", feature = "mobile")))]
fn main() {
    dioxus::launch(web::App);
}

/// The Android app: the same window, its line native (`android`).
#[cfg(all(feature = "mobile", not(feature = "server")))]
fn main() {
    dioxus::launch(android::App);
}
