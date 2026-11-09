//! `edualcd`: edualc's daemon, and the server that makes edualc's window a page.
//!
//! - It runs Claude Code, sandboxed, for edualc's windows, reads its files for them and does
//!   what they ask (`edualc-host`), for windows on this machine and others alike: each reaches
//!   it at `/api/line` (`line`), over TLS with the certificate edualcd made itself, and shows
//!   the token edualcd keeps (`state`), which lets whoever holds it run Claude Code as the user.
//!   It runs as a user service (`edualcd.service`) and keeps its sessions while windows come and
//!   go.
//! - It is a Dioxus fullstack server: it serves the page (`web`), the same window as the native
//!   one, drawn by Dioxus from the same presenter, whose line is a window's like any other.
//! - `edualcd --decode-image` and `--show-image` are the confined image decoder (see `decode`).
//!
//! Built with the `mobile` feature instead (`dx build --android`), it is the Android app
//! (`android`): the same window, which reaches an edualcd as the native one does.

#[cfg(feature = "server")]
mod serve;
#[cfg(feature = "server")]
mod state;

#[cfg(feature = "mobile")]
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
