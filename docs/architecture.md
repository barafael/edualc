# How edualc is built

## Layout

edualc is a Cargo workspace of four crates:

| | |
|---|---|
| `crates/core` (`edualc-core`) | what every window shares: the conversation, the app that owns its state, what of it a window shows (`present`), the protocol and the line to the daemon. Builds for Linux and for `wasm32` (a page); what only Linux has is behind `#[cfg(not(target_arch = "wasm32"))]` |
| `crates/host` (`edualc-host`) | the daemon: runs `claude` (sandboxed) for the windows, reads Claude Code's files for them, does what the desktop is asked; and the demo's scripted Claude (`demo`) |
| `crates/slint` (`edualc`) | the native window: Slint (`ui/*.slint`), its frontend of the presenter (`src/ui/`), its confinement and its line to the daemon on the Unix socket |
| `crates/server` (`edualc-server`, the `edualcd` binary) | the daemon's program (`edualcd --daemon`), and a Dioxus fullstack server: the same window as a page (`src/web/`), whose lines it carries on to the daemon |

Within them:

| | |
|---|---|
| `core/src/app.rs`, `core/src/app/` | `App`, the one owner of all state; UI actions, the daemon's messages, timers (`repeat`) and what was read reach it as `Event`s on one channel |
| `core/src/present/` | the `Presenter`: the app's state made into what a window shows, as plain data (`model.rs`, generated from `ui/types.slint` and `ui/state.slint` by `scripts/present_gen.py`), handed to a `Frontend` once per batch, each part only when it changed |
| `core/src/ipc.rs`, `core/src/ipc/` | the messages between the windows and the daemon, its private socket with its checks (`socket`), WebSocket framing (`ws`), and reaching (starting) the daemon (`reach`) |
| `core/src/link.rs` | a window's line, whatever carries it: the channel to whatever writes it, the daemon's greeting, the `Inbox` that makes its messages the app's events |
| `core/src/claude.rs` | a session's process as the window sees it |
| `core/src/chat.rs`, `tools.rs`, `markdown.rs`, `plan.rs`, `grouping.rs`, `sidebar.rs` | the conversation, how tool calls, Markdown and plans read, runs of tool calls folded, the session list |
| `core/src/sessions.rs`, `trust.rs`, `settings.rs`, `sandbox.rs`, `home.rs`, `process.rs`, `desktop.rs` | Claude Code's files, trust, the settings, the Landlock sandbox; their reading and writing is the daemon's |
| `core/src/decode.rs`, `decode/` | images made safe to show and send, and the confined process that decodes them |
| `core/src/external/` | formats other programs write, as types; they take keys they don't know |
| `host/src/daemon.rs`, `spawn.rs`, `relay.rs`, `mirror.rs`, `buses.rs` | the daemon's actors (sans-io cores), starting `claude`, the session messaging relay, the sandboxed transcript's mirror, the filtering D-Bus proxies |
| `host/src/files.rs` | what the daemon reads and watches for windows (inotify), and keeping the settings |
| `slint/src/start.rs`, `connect.rs`, `confine.rs`, `ui/` | the window's start, its line, its confinement, its Slint frontend and callbacks |
| `server/src/serve.rs`, `line.rs`, `web/` | edualcd's command line, the page's line (a Dioxus server function), the page: its frontend (`web/view.rs`) and components |
| `ui/*.slint`, `assets/` | the native window, the heron and the app's icon |

## The protocol

The windows and the daemon speak WebSocket: each message one binary frame of CBOR (the
framing Dioxus's typed websockets use), `ToDaemon` one way and `FromDaemon` the other, checked
strictly (unknown fields refused), versioned by `ipc::PROTOCOL` in the hello. The native window
speaks it on the daemon's Unix socket; a page speaks it to edualcd's server (`/api/line`), which
carries each message on to the daemon's socket unchanged in meaning. Besides starting, feeding
and attaching to `claude` processes and asking the desktop (`Act`), a window asks the daemon to
read what it doesn't read itself (`Read`: the running sessions, a folder's sessions, transcripts,
worktrees, folders) and to watch what changes (`Watch`); a transcript's lines come raw, in
pieces (`Lines`), and are parsed by the window. Neither the page nor the native window reads
`~/.claude`.

## The page

The page is the same app, built for `wasm32`: the presenter's frontend there writes Dioxus
signals, which the components (`server/src/web/components/`, one per area of `ui/app.slint`)
draw. `dx build -p edualc-server --platform web --fullstack` builds it; `edualcd` serves it, at
`--listen ADDRESS` or Dioxus's address (127.0.0.1:8080), and prints its address with a token:
`EDUALC_TOKEN`'s, or a random one. It is never taken on the command line, which every user of
the machine can read, and the daemon is started without it, so nothing Claude Code runs can
reach the pages' server with it. The server lets a page's line through only with that token
(compared in constant time): any user of the machine reaches the loopback, and a site in the
browser could try it. Keep the
server on the loopback; the line is not encrypted. What a page asks of the desktop (opening a
folder in an editor, the folder picker, the clipboard's image) happens on the machine the
daemon runs on.

## The demo

`edualcd --demo` serves the page with a script in place of the daemon and Claude Code
(`host/src/demo`): its scenarios answer as `claude -p` would, from a word in the prompt, and the
demo project's sessions (`web/demo/`, as Claude Code writes them) stand in for `~/.claude`.
Nothing runs and nothing on the disk is read, so it needs no token.

The window doesn't run Claude Code itself: the first window starts `edualcd --daemon` (beside it), which runs
the `claude` processes and does what the desktop is asked (opening editors, notifications,
stopping sessions elsewhere), and ends a few seconds after its last window and session. They
speak over a Unix socket in `$XDG_RUNTIME_DIR/edualc/protocol-<n>`, a folder for each version
of what they say, so a new build's windows start a daemon of their own while an earlier one
finishes its sessions: a folder of the user's alone, checked through its descriptor and never
through a link; a lock deciding which process is the daemon,
so no two bind and none unbinds another's; peers checked to be this user running edualc (the window's or edualcd's program, from the same
folder); messages as above, a frame bigger than `ipc::MAX_MESSAGE` ending the connection. The daemon's parts are
actors in the manner of "More Actors with Tokio" (each its data, run by an event loop it is
given its channels for, channels without cycles, shutting down as they close), with sans-io
logic tested without sockets or processes.

The window confines itself before it makes any thread: it reads the system and the user's
files but their secrets (keys, keyrings, browser profiles, credentials, `~/.claude.json` with
its MCP servers' environments: the daemon reads the trust kept there), writes only its
settings and caches, reaches no socket but Wayland's and the daemon's folder, opens no network
socket at all (seccomp), and gains no privileges. Its D-Bus is two `xdg-dbus-proxy` filters the
daemon runs, as Flatpak does: on the session bus only accessibility's address and the desktop's
light or dark setting are reachable (not systemd, which starts programs, nor the keyring); on
the accessibility bus a screen reader can read the window, but the window can't read or drive
other programs.

Nothing runs while nothing happens: there is no animation loop and no polling (files other
processes write are watched with inotify; a check once a minute catches sessions that crashed).
`SLINT_DEBUG_PERFORMANCE=refresh_lazy,console` prints the frames drawn.

The code follows the conventions in `../edit-epistemology/benchmark/harness/CONVENTIONS.md`: no
shared mutable state (`clippy.toml` refuses cells, `Rc` and locks), typed errors, nothing discarded,
and that workspace's lint set (`Cargo.toml`) and ast-grep rules (`lint/`; formats edualc owns
refuse unknown keys, those in `src/external/` take them). `cargo clippy --workspace --exclude
edualc-server --all-targets -- -D warnings`, `cargo clippy -p edualc-server --features server
-- -D warnings`, the page's `cargo clippy --target wasm32-unknown-unknown -p edualc-core -p
edualc-server --features edualc-server/web -- -D warnings -A dead_code`, `ast-grep scan`,
`cargo deny check` (licenses that combine with the GPL, advisories, sources) and `cargo test`
are clean. Use the toolchain CI pins (1.99.1).

## Releasing

A release is a tag: `v` and the version in `Cargo.toml`, pushed, builds the packages
(`.github/workflows/release.yml`), installs and starts each on Debian, Ubuntu, Fedora and
AlmaLinux, and drafts the release for a person to publish.

