//! `edualcd`: edualc's daemon and the server that makes edualc's window a page.
//!
//! - `edualcd --daemon` is the daemon (`edualc-host`): it runs Claude Code, sandboxed, for the
//!   windows, reads its files for them and does what the desktop is asked, on a Unix socket
//!   only edualc's programs of this user reach. The first window (or page server) starts it,
//!   and it ends a few seconds after its last window and session.
//! - `edualcd` is a Dioxus fullstack server: it serves the page (`web`), the same window as the
//!   native one, drawn by Dioxus from the same presenter, and carries each page's line to the
//!   daemon (`line`). A page proves itself with the token the server prints, so that no other
//!   user of the machine, nor a site in the browser, drives Claude Code through it.
//! - `edualcd --decode-image` and `--show-image` are the confined image decoder (see `decode`).

#[cfg(feature = "server")]
mod serve;

mod line;
mod web;

#[cfg(feature = "server")]
fn main() -> std::process::ExitCode {
    serve::main()
}

#[cfg(not(feature = "server"))]
fn main() {
    dioxus::launch(web::App);
}
