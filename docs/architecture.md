# How edualc is built

## Layout

edualc is a Cargo workspace of four crates:

| | |
|---|---|
| `crates/core` (`edualc-core`) | what every window shares: the conversation, the app that owns its state, what of it a window shows (`present`), the protocol and the line to edualcd (TLS, pairing). Builds for Linux and for `wasm32` (a page); what only Linux has is behind `#[cfg(not(target_arch = "wasm32"))]` |
| `crates/host` (`edualc-host`) | the daemon: runs `claude` (sandboxed) for the windows, reads Claude Code's files for them, stops sessions running elsewhere; and the demo's scripted Claude (`demo`) |
| `crates/slint` (`edualc`) | the native window: Slint (`ui/*.slint`), its frontend of the presenter (`src/ui/`), its desktop (`seat`: links, notifications, the clipboard), its D-Bus proxies, its confinement, its paired edualcds and its line to one |
| `crates/server` (`edualc-server`, the `edualcd` binary) | edualcd: the daemon run in a Dioxus fullstack server, over TLS, which also serves the same window as a page (`src/web/`) |

Within them:

| | |
|---|---|
| `core/src/app.rs`, `core/src/app/` | `App`, the one owner of all state; UI actions, edualcd's messages, timers (`repeat`) and what was read reach it as `Event`s on one channel |
| `core/src/present/` | the `Presenter`: the app's state made into what a window shows, as plain data (`model.rs`, generated from `ui/types.slint` and `ui/state.slint` by `scripts/present_gen.py`), handed to a `Frontend` once per batch, each part only when it changed; and what the frontend does on its own desktop (`Seat`) |
| `core/src/ipc.rs`, `core/src/ipc/` | the messages between the windows and edualcd, the window's end of the line (`ws`), its TLS with the certificate pinned (`tls`), and the address a window pairs with (`pairing`) |
| `core/src/link.rs` | a window's line, whatever carries it: the channel to whatever writes it, the daemon's greeting, the `Inbox` that makes its messages the app's events |
| `core/src/claude.rs` | a session's process as the window sees it |
| `core/src/chat.rs`, `tools.rs`, `markdown.rs`, `plan.rs`, `grouping.rs`, `sidebar.rs` | the conversation, how tool calls, Markdown and plans read, runs of tool calls folded, the session list |
| `core/src/sessions.rs`, `trust.rs`, `settings.rs`, `sandbox.rs`, `home.rs`, `process.rs`, `desktop.rs` | Claude Code's files, trust, the settings, the Landlock sandbox; their reading and writing is the daemon's |
| `core/src/decode.rs`, `decode/` | images made safe to show and send, and the confined process that decodes them |
| `core/src/external/` | formats other programs write, as types; they take keys they don't know |
| `host/src/daemon.rs`, `spawn.rs`, `relay.rs`, `mirror.rs` | the daemon's actors (sans-io cores), starting `claude`, the session messaging relay, the sandboxed transcript's mirror |
| `host/src/files.rs` | what the daemon reads and watches for windows (inotify), and keeping the settings |
| `slint/src/start.rs`, `connect.rs`, `servers.rs`, `buses.rs`, `seat.rs`, `confine.rs`, `ui/` | the window's start, its line (and reaching it again), its paired edualcds, its D-Bus proxies, its desktop, its confinement, its Slint frontend and callbacks |
| `server/src/serve.rs`, `state.rs`, `line.rs`, `web/` | edualcd's command line, TLS and the lines it hands the daemon; its certificate and token; the line's route; the page: its frontend (`web/view.rs`) and components |
| `ui/*.slint`, `assets/` | the native window, the heron, the app's icon, edualcd's user service |

## The protocol

The windows and edualcd speak WebSocket over TLS: each message one binary frame of CBOR,
`ToDaemon` one way and `FromDaemon` the other, checked strictly (unknown fields refused),
versioned by `ipc::PROTOCOL`, which goes in the line's address too, so that an edualcd of another
version refuses a window (HTTP 409) before either reads the other's messages. A native window
and a page reach edualcd alike, at `/api/line?token=…&protocol=…`, whether it runs on their
machine or another: the window on this machine is no special case. Besides starting, feeding and
attaching to `claude` processes and asking edualcd's machine (`Act`: whether the terminal's trust
dialog was accepted, stopping a session running elsewhere), a window asks edualcd to read what
it doesn't read itself (`Read`: the running sessions, a folder's sessions, transcripts,
worktrees, folders, where a session was started) and to watch what changes (`Watch`); a
transcript's lines come raw, in pieces (`Lines`), and are parsed by the window. Neither the page
nor the native window reads `~/.claude`, and paths in the messages are edualcd's machine's.

What is the user's desktop's is the window's to do (`present::Seat`, which each frontend
carries out): links open in its browser, notifications show on it, an image is pasted from its
clipboard. The native window opens links through the desktop portal (`OpenURI`), outside its
sandbox, and shows notifications through the notification service, both through its D-Bus
proxies; it reads the clipboard with `wl-paste` and decodes in its confined helper. The page uses
the browser's own.

## edualcd

edualcd is one long-running process per user, a user service (`assets/edualcd.service`): the
daemon (`edualc-host`'s hub, its sessions, the files it reads and watches) runs in it, and each
window's line is handed to it as channels (`daemon::Daemon::window`). Its parts are actors in the
manner of "More Actors with Tokio" (each its data, run by an event loop it is given its channels
for, channels without cycles, shutting down as they close), with sans-io logic tested without
sockets or processes. A window's outbox is unbounded, so that a window slow to read holds up
neither the hub nor a session; its line is pinged every 15 s and let go of after 45 s of
silence, and what was queued for it with it. Sessions outlive their windows: a window gone
leaves them running, and a window that comes again (its network lost the line) names itself
with the key it keeps (`ipc::WindowKey`), so that the sessions its lost line showed are let go
of at once for it to take back.

On its first start edualcd makes a certificate for itself (rcgen) and a token, and keeps them
in `$XDG_STATE_HOME/edualcd`, a folder of the user's alone; `EDUALC_TOKEN` can give the token
instead, which is never taken on the command line (every user of the machine reads that) and is
taken out of each session's environment. `edualcd --pair` prints the address windows pair with
and pages open: `https://host:port/#token=…&fingerprint=…`. A native window keeps the edualcds it
paired with in `~/.config/edualc/servers.json` (the user's alone) and trusts exactly the
certificate paired with, by its SHA-256, whatever it names or is signed by (`ipc::tls`); a
browser asks the user first. The token lets whoever holds it run Claude Code as the user, outside
the sandbox too: the server compares it in constant time, refuses a wrong one (HTTP 403) before
the WebSocket opens, and edualcd makes itself non-dumpable, which closes its memory and
environment to the user's other processes; Claude Code's sandbox reaches neither the token's
file nor the window's.

The window reaches its edualcd again by itself when the line is lost, at the addresses it looked
up before it was confined, waiting longer after each failure; the app opens its session again
from the transcript, and what it asked in between is dropped. edualcd restarting (to update it)
ends the sessions it runs; their conversations are kept, to be continued.

## The page

The page is the same app, built for `wasm32`: the presenter's frontend there writes Dioxus
signals, which the components (`server/src/web/components/`, one per area of `ui/app.slint`)
draw. `dx build -p edualc-server --platform web --fullstack` builds it; `edualcd` serves it over
TLS, at `--listen ADDRESS` (127.0.0.1:7878 by default), and under `dx serve` at Dioxus's address
over plain HTTP, for working on it.

## The demo

`edualcd --demo` serves the page, over plain HTTP, with a script in place of the daemon and
Claude Code (`host/src/demo`): its scenarios answer as `claude -p` would, from a word in the
prompt, and the demo project's sessions (`web/demo/`, as Claude Code writes them) stand in for
`~/.claude`. Nothing runs and nothing on the disk is read, so it needs no token.

## The window

The window doesn't run Claude Code itself. Its first process starts the window's D-Bus proxies
(two `xdg-dbus-proxy` filters, as Flatpak does: on the session bus only accessibility's address,
the desktop's light or dark setting, the portal that opens links and the notification service
are reachable, not systemd, which starts programs, nor the keyring; on the accessibility bus a
screen reader can read the window, but the window can't read or drive other programs), then the
window as its child, its bus the proxies', and waits for it; the window ends with it.

The window confines itself before it makes any thread, once it has reached its edualcd: it
reads the system and the user's files but their secrets (keys, keyrings, browser profiles,
credentials, `~/.claude.json` with its MCP servers' environments), writes only its settings and
caches, reaches no socket file but Wayland's and its proxies', connects by TCP to edualcd's port
alone (Landlock), opens no UDP, raw or other network socket (seccomp; so no DNS either), and
gains no privileges.

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

