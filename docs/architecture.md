# How edualc is built

## Layout

| | |
|---|---|
| `src/start.rs` | how the program starts on Linux: its command line, the daemon reached, the window opened |
| `src/claude.rs` | a session's process as the window sees it: what it is started with, what its lines mean, the handle that writes to it |
| `src/daemon.rs`, `src/spawn.rs` | the daemon (`edualc --daemon`): runs `claude`, or agy's adapter, for the windows, and does what the desktop is asked; actors with sans-io cores |
| `src/agy.rs`, `src/agy/` | an agy session: the adapter (`edualc --agy-adapter`) that speaks the Agent Client Protocol to `agy acp` and stream-json to the daemon, agy's data folder per project, its inbox, and the messages it sends into other sessions' |
| `src/ipc.rs`, `src/ipc/socket.rs`, `src/link.rs` | the messages between the window and the daemon, the daemon's private socket with its checks and bounded frames (postcard), and the window's line to it |
| `src/decode.rs`, `src/decode/helper.rs` | images made safe to show and send, and the confined process that decodes them |
| `src/external/` | formats other programs write, as types: Claude Code's stream-json, transcripts and files, and niri's IPC; they take keys they don't know, as each release adds some |
| `src/app.rs` | `App`, the one owner of all state; UI callbacks, process output, timers and background work reach it as `Event`s on one channel |
| `src/app/protocol.rs` | what the process says, taken into the state |
| `src/app/side.rs` | side questions (`/btw`) |
| `src/app/worktree.rs` | a session's worktree, and opening it |
| `src/chat.rs` | the conversation as entries, live or from a transcript |
| `src/tools.rs`, `src/markdown.rs`, `src/plan.rs` | how tool calls, Markdown and plan comments read |
| `src/grouping.rs` | runs of tool calls, folded into one line |
| `src/sessions.rs` | saved sessions (`~/.claude/projects`) and running ones (`~/.claude/sessions`) |
| `src/sidebar.rs` | the sidebar's list: running sessions (the window's own among them), then this folder's earlier ones |
| `src/watcher.rs` | inotify: noticing the running sessions' records and a followed transcript change |
| `src/trust.rs` | which folders are trusted: `claude -p` skips the terminal's trust dialog, so the window asks |
| `src/process.rs` | what `/proc` says about a process (to tell a session's process from a reused pid) |
| `src/desktop.rs` | notifications, other programs' windows (through niri's socket), and what folders open in |
| `src/appearance.rs`, `src/settings.rs` | fonts, the desktop's text size, the kept zoom |
| `src/ui/` | the Slint models kept in line with the state, and the callbacks |
| `ui/*.slint` | the window |
| `src/web/`, `web/` | the browser demo: the daemon as a script, the demo project's sessions, and the page with its fonts |
| `assets/` | the heron (standing, its idle poses, and three wing beats for its flight), the app's icon and its desktop entry |

## The browser demo

The same window builds for `wasm32-unknown-unknown` and runs in a page
(`.github/workflows/pages.yml` publishes it to GitHub Pages). What only Linux has, the daemon,
its sockets and processes, is left out of that build (`#[cfg(not(target_arch =
"wasm32"))]`), and the window's few ties to it get browser versions in `src/web/`, behind the
same interfaces: `link` leads to a script in the page (`src/web/daemon.rs`) that answers as
`claude -p` would, from scenarios a word in the prompt picks (`src/web/scenarios.rs`); `watcher`
never fires; images are decoded where they are shown, the page being the sandbox; background
work is done at once, as a page has no threads. `sessions` reads the demo project's
transcripts (`web/demo/`, as Claude Code writes them) in place of `~/.claude`. Everything else,
`App`, the chat, Markdown, tool cards, plan review and the view, is the code that runs on Linux.

`cargo clippy --target wasm32-unknown-unknown -- -D warnings -A dead_code` checks that build
(CI does); dead code is allowed there, as what only the daemon calls goes unused.

The window doesn't run Claude Code itself: the first window starts `edualc --daemon`, which runs
the `claude` processes and does what the desktop is asked (opening editors, notifications,
stopping sessions elsewhere), and ends a few seconds after its last window and session. They
speak over a Unix socket in `$XDG_RUNTIME_DIR/edualc/protocol-<n>`, a folder for each version
of what they say, so a new build's windows start a daemon of their own while an earlier one
finishes its sessions: a folder of the user's alone, checked through its descriptor and never
through a link; a lock deciding which process is the daemon,
so no two bind and none unbinds another's; peers checked to be this user running this program;
messages framed by postcard's COBS, read into a fixed buffer that ends the connection when a
frame doesn't fit. The daemon's parts are
actors in the manner of "More Actors with Tokio" (each its data, run by an event loop it is
given its channels for, channels without cycles, shutting down as they close), with sans-io
logic tested without sockets or processes.

edualc puts no sandbox around the agents it runs: Claude Code and agy run with the user's
rights, as in a terminal, and their own sandboxes and settings apply as there. The daemon
isn't confined either: Landlock, seccomp, `no_new_privs` and a systemd unit's hardening pass to
every process it starts, and `no_new_privs` breaks `sudo` in an agent's shell. What is edualc's
own stays private: the socket above, its
state (0600 files in 0700 folders), no network listener, and images decoded in a process of
their own that confines itself before it reads them (`decode`).

Nothing runs while nothing happens: there is no animation loop and no polling (files other
processes write are watched with inotify; a check once a minute catches sessions that crashed).
`SLINT_DEBUG_PERFORMANCE=refresh_lazy,console` prints the frames drawn.

The code follows the conventions in `../edit-epistemology/benchmark/harness/CONVENTIONS.md`: no
shared mutable state (`clippy.toml` refuses cells, `Rc` and locks), typed errors, nothing discarded,
and that workspace's lint set (`Cargo.toml`) and ast-grep rules (`lint/`; formats edualc owns
refuse unknown keys, those in `src/external/` take them). `cargo clippy --all-targets -- -D
warnings`, `ast-grep scan`, `cargo deny check` (licenses that combine with the GPL, advisories,
sources) and `cargo test` are clean.

## Releasing

A release is a tag: `v` and the version in `Cargo.toml`, pushed, builds the packages
(`.github/workflows/release.yml`), installs and starts each on Debian, Ubuntu, Fedora and
AlmaLinux, and drafts the release for a person to publish.

