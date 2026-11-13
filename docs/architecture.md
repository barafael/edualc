# How edualc is built

## The window and the daemon

The window doesn't run Claude Code itself. The first window you open starts a daemon,
`edualcd`, installed beside it, and later windows connect to it. The daemon:

- runs the `claude` processes, or agy's adapter, for every window;
- reads Claude Code's files for the windows, and watches them for changes;
- does what the window asks of the desktop: opening editors, sending notifications, stopping
  sessions that run elsewhere;
- ends five seconds after its last window and session have gone.

The window and the daemon talk over a Unix socket in `$XDG_RUNTIME_DIR/edualc/protocol-<n>/`.
Each version of their protocol gets its own folder. So a newly built window starts a daemon of its
own, while an older daemon finishes its sessions.

The socket is guarded in four ways:

- **Its folder must be yours alone.** The daemon checks the folder through its file descriptor and
  never follows a link to it.
- **A lock decides which process is the daemon,** so no two processes bind the socket and none
  removes another's.
- **Every peer must be you, running edualc,** checked with the kernel's peer credentials and the
  program's path: the daemon itself, or a program installed beside it.
- **Messages are framed with postcard's COBS** and read into a fixed buffer. A frame that doesn't
  fit ends the connection.

The window doesn't read Claude Code's files either. It asks the daemon (`Read`): for the running
sessions, a folder's sessions, the projects, a transcript, a worktree or a folder. The daemon sends
the settings with its greeting, and tells the window when something it watches changes (`Watch`).
A window that isn't a Linux program, such as a web page, couldn't read those files. A transcript
comes as text in pieces (`Lines`), and the window splits and parses it. Every other message fits
in `ipc::MAX_MESSAGE`, which is enough for a thousand sessions.

The daemon's parts are actors, in the manner of "More Actors with Tokio". Each owns its data and
runs an event loop on the channels it is given. The channels have no cycles, and an actor shuts
down when its channels close. The logic is sans-io, so it's tested without sockets or processes.

## No sandbox

edualc doesn't sandbox the agents it runs. Claude Code and agy run with your rights, as in a
terminal, and their own sandboxes and settings apply as they do there.

The daemon isn't confined either. Landlock, seccomp, `no_new_privs` and a systemd unit's hardening
would all pass on to every process the daemon starts, and `no_new_privs` would break `sudo` in an
agent's shell.

edualc keeps its own parts private:

- the socket described above;
- its state, in files with mode 0600 inside folders with mode 0700;
- no network listener, unless you start edualcd with `--listen` (see below);
- images, decoded in a process of their own that locks itself down before it reads them (`decode`).

## The page

The page is the same app, built for `wasm32`. Its frontend of the presenter
(`web::view::PageFrontend`) writes Dioxus signals, which the components
(`crates/server/src/web/components/`) draw, and what you do goes back as `App`'s actions.

The page is served by the edualcd it talks to (`edualcd --listen`), and connects to it over the
network (see below), at the host the page came from. It gets the token in its address's fragment
(`#token=…`), which is never sent to a server.

The browser is the page's desktop: links open in it, notifications show through it, and a pasted
image comes from its clipboard (`present::Seat`). What only the desktop of edualcd's machine could
do isn't offered: editors, other programs' windows, a folder picker. You type a folder's path on
edualcd's machine instead. `edualcd --demo` serves the page over plain HTTP to anyone, with a script
in place of Claude Code.

`dx` builds the page (`just release-build`). Its files (`public/`, whose names carry their hashes)
are served from `DIOXUS_PUBLIC_PATH`, or from `public` beside edualcd's program. A package puts them
in `/usr/share/edualc/public`, and the user service says so.

## The network side

Windows that aren't on this machine, such as a web page or an app on another device, reach
edualcd over the network. This is in addition to the Unix socket: a window on this machine still
uses the socket, with its checks.

`edualcd --listen ADDRESS` accepts connections at `/api/line` over TLS. They carry WebSocket
frames of what the socket carries (postcard's COBS), and each is one more window of the same
daemon. A user service, `assets/edualcd.service`, runs it on the loopback address. The daemon
gets each connection as a `daemon::Line`, so another transport, such as WebTransport, would only
need to provide one.

On its first start, edualcd makes a certificate for itself (with rcgen) and a token, and keeps them
in `$XDG_STATE_HOME/edualcd`, a folder that is yours alone. `EDUALC_TOKEN` can set the token
instead. The token is never taken on the command line, since every user of the machine can read
that. `edualcd --pair` prints the address windows pair with:
`https://host:port/#token=…&fingerprint=…`. The token is in the fragment, which is never sent to a
server. A window trusts exactly the certificate it paired with, by its SHA-256, whatever it names
or is signed by.

The token lets whoever holds it run Claude Code as you, so it is kept like an SSH key:

- it's held in memory that is cleared when it goes (`ipc::Token`), and `Debug` never shows it;
- it's compared in constant time (by the SHA-256 of each);
- it's never in an address, since proxies and servers log those. A connection shows it in its first
  frame, of at most 1 KiB and within 5 seconds, or is closed with nothing else read.

A connection that speaks another protocol is refused (HTTP 409) before the WebSocket opens. A
connection is pinged every 15 seconds and dropped after 45 seconds in which nothing came from it.

Only a window on this machine may act on its desktop. From a connection over the network the
daemon refuses it (`ActFailure::NotHere`), and its hello says the window isn't local (`local`, no
editors, no windows to find), so the window does what it can on its own desktop instead.

## Doing nothing while nothing happens

There is no animation loop. The daemon notices through inotify when files other processes write
change, and tells the window. Nothing is checked on a schedule, with two exceptions:

- once a minute, the window checks the running sessions, to catch one that crashed and left its
  record behind;
- while you follow a background command in the side panel, its output is fetched every second.

`SLINT_DEBUG_PERFORMANCE=refresh_lazy,console` prints every frame drawn.

## Layout

edualc is a Cargo workspace of four crates:

| | |
|---|---|
| `crates/core` (`edualc-core`) | what every window shares: the conversation, `App` that owns its state, what a window shows (`present`), the protocol, and the window's connection to the daemon. It builds for Linux and for `wasm32` (the browser demo); what only Linux has is behind `#[cfg(not(target_arch = "wasm32"))]`. |
| `crates/host` (`edualc-host`) | the daemon: it runs `claude` or agy's adapter, reads Claude Code's files, and acts on the desktop |
| `crates/server` (`edualc-server`, the `edualcd` binary) | edualcd: the daemon with its command line, and the Dioxus fullstack server that serves the window as a web page, drawn from the same presenter |
| `crates/slint` (`edualc`) | the window, drawn with Slint, and the browser demo |

In `crates/core/src`:

| | |
|---|---|
| `app.rs`, `app/` | `App`, the single owner of all state. Your actions, the daemon's messages, timers (`repeat`) and what the daemon read reach it as `Event`s on one channel. `app/` holds applying what the process says (`protocol`), the side panel (`side`, `peek`), what runs beside the turn (`tasks`), the usage panel (`usage`), images, and worktrees. |
| `present/` | the `Presenter`: the state turned into what a window shows, as plain data (`model.rs`, generated from `ui/types.slint` and `ui/state.slint` by `scripts/present_gen.py`), handed to a `Frontend` once per batch, each part only when it changed |
| `demo/` | the demo: Claude Code played by a script, on a demo project, for `edualcd --demo` and the Slint window's browser demo (the `demo` feature) |
| `colour.rs` | what a window that colours code is asked and tells back: a block to colour, and the same block coloured |
| `ipc.rs`, `ipc/socket.rs` | the messages between the window and the daemon, and the daemon's socket |
| `ipc/pairing.rs`, `ipc/tls.rs`, `ipc/token.rs` | the address a window pairs with, edualcd's TLS (its own certificate, pinned by its SHA-256), and the token a connection over the network shows |
| `link.rs` | the window's connection, whatever carries it: the channel its messages go on, the daemon's greeting, and the `Inbox` that turns the daemon's messages into `Event`s |
| `claude.rs` | a session's process as the window sees it: what it is started with, what its output means, and the handle that writes to it |
| `chat.rs`, `tools.rs`, `markdown.rs`, `plan.rs`, `grouping.rs`, `sidebar.rs` | the conversation as entries; how tool calls, Markdown and plan comments are shown; runs of tool calls folded into one line; the sidebar's list |
| `sessions.rs`, `trust.rs`, `settings.rs`, `folder.rs`, `process.rs`, `desktop.rs` | Claude Code's files, which folders are trusted, the settings, checking a folder you name, what `/proc` says about a process, notifications, other programs' windows and what a folder opens in. The daemon does the reading and writing. |
| `decode.rs`, `decode/` | making images safe to show and send, and the locked-down process that decodes them |
| `external/` | formats other programs write, as types: Claude Code's stream-json, transcripts and files, the Agent Client Protocol, and niri's IPC. These accept keys they don't know, since each release adds some. |
| `cjk.rs` | which language a text's Han characters are in |

In `crates/host/src`:

| | |
|---|---|
| `daemon.rs`, `spawn.rs` | the daemon's actors, starting `claude` or agy's adapter, and acting on the desktop |
| `files.rs` | what the daemon reads and watches for windows (with inotify), and saving the settings |
| `net/` | the daemon's network side: its certificate and token (`state`), the TLS listener, and the route a connection comes in by (`line`), which lets it in by its first frame |
| `agy.rs`, `agy/` | an agy session: the adapter (`edualcd --agy-adapter`) that speaks the Agent Client Protocol to `agy acp` and Claude Code's stream-json to the daemon; agy's data folder per project; its inbox; the messages it sends to other sessions |
| `demo/` | `edualcd --demo`: the demo's daemon, which answers a connection as the daemon does |

In `crates/server/src`:

| | |
|---|---|
| `main.rs`, `serve.rs` | edualcd's command line, and starting the daemon, its network side and the page |
| `web/` | the page: its frontend of the presenter (`web/view.rs`), and its components, one for each area of `ui/app.slint` |
| `line.rs` | the page's connection to edualcd |

In `crates/slint/src`:

| | |
|---|---|
| `main.rs`, `start.rs`, `start/manual.rs` | how the window starts: its command line, reaching or starting the daemon, opening the window; the manual page |
| `connect.rs` | the window's connection to the daemon over its Unix socket |
| `ui/` | the Slint frontend of the presenter: keeping the Slint models in line with the state, and the callbacks |
| `highlight.rs`, `colouring.rs` | colouring code by its language, on a thread of its own |
| `appearance.rs` | fonts and the desktop's text size |
| `words.rs` | text that isn't a closed set of ours: words people read, or someone else's |
| `web/` | the browser demo: a scripted daemon and the demo project's sessions |

Elsewhere:

| | |
|---|---|
| `ui/*.slint` | the window |
| `web/` | the browser demo's page, with its fonts |
| `assets/` | the heron (standing, its idle poses, and its wing beats), the app's icon, desktop entry, metainfo and manual page |
| `dist/` | packaging: one description in Nickel (`package.ncl`) that the deb, rpm and Arch files are written from |

## The presenter

`App` owns all state and says what a window shows as plain data (`present::model`). The
`Presenter` remembers what it told the frontend, and tells it only what changed. The Slint window
is a frontend (`crates/slint/src/ui/`): it draws the data as Slint's types, changes a list row by
row so a row that stays keeps its selection, and turns its callbacks into `App`'s actions.

A frontend may do some things on its own. The Slint window colours a conversation's code on a
thread of its own, and draws the block again when the colours come back (`Event::CodeColoured`,
`Frontend::code_coloured`). It also puts back the space Slint leaves out between a text's
paragraphs, so the Markdown every frontend gets stays plain.

## Conventions

- **No shared mutable state.** `App` owns the state, and everything else reaches it by message.
  `clippy.toml` refuses cells, `Rc`, `Arc` and locks.
- **Errors are typed,** and nothing is silently discarded.
- **Lints:** the lint set in `Cargo.toml` and the ast-grep rules in `lint/`. Formats edualc owns
  refuse unknown keys; those in `crates/core/src/external/` accept them.
- **Checks:** `cargo clippy --all-targets -- -D warnings`, `ast-grep scan`, `cargo deny check`
  (licences that combine with the GPL, advisories, sources) and `cargo test` all pass. CI runs
  them on every push to main and on every pull request.

## The browser demo

The same window builds for `wasm32-unknown-unknown` and runs in a web page.
`.github/workflows/pages.yml` publishes it to GitHub Pages.

The browser has no daemon, sockets or processes, so the build leaves them out
(`#[cfg(not(target_arch = "wasm32"))]`). The few parts of the window that use them have browser
versions in `crates/slint/src/web/`, with the same interfaces:

- `link` leads to a script in the page (`crates/slint/src/web/daemon.rs`) that answers as
  `claude -p` would. A word in your prompt picks the scenario it plays
  (`crates/slint/src/web/scenarios.rs`).
- The script also answers what the window asks the daemon to read (the sessions, the projects, a
  transcript) from the demo project's transcripts (`web/demo/`, as Claude Code writes them)
  instead of `~/.claude`.
- Images are decoded where they're shown, since the page is the sandbox.
- Background work is done at once, since a page has no threads.

Everything else is the code that runs on Linux: `App`, the conversation, Markdown, tool cards,
plan review and the view.

`cargo clippy -p edualc-core -p edualc --target wasm32-unknown-unknown -- -D warnings -A dead_code` checks this build, and
CI runs it. Dead code is allowed there, because code that only the daemon calls is unused in the
browser.

## Releasing

A release starts with a tag: `v` followed by the version in `Cargo.toml`. Pushing it starts
`.github/workflows/release.yml`, which:

1. builds the tarball, `.deb` and `.rpm` for x86_64 and aarch64;
2. installs and starts each package on Debian, Ubuntu, Fedora and AlmaLinux;
3. drafts a GitHub release for a person to publish.

After that, `just release-checksum` puts the tarball's checksum in `dist/releases.json` and writes
the release's PKGBUILD in `dist/aur/edualc`.
