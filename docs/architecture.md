# How edualc is built

## Layout

| | |
|---|---|
| `src/claude.rs` | a session's process as the window sees it: what it is started with, what its lines mean, the handle that writes to it |
| `src/daemon.rs`, `src/spawn.rs` | the daemon (`edualc --daemon`): runs `claude` (sandboxed) for the windows, and does what the desktop is asked; actors with sans-io cores |
| `src/confine.rs`, `src/buses.rs` | the window's own confinement (Landlock, a seccomp socket filter), and the filtering D-Bus proxies it gets in place of the session bus |
| `src/ipc.rs`, `src/link.rs` | the daemon's private socket, its checks and bounded messages (postcard), and the window's line to it |
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
| `src/sandbox.rs`, `src/trust.rs` | the Landlock sandbox for `claude`, and which folders are trusted |
| `src/process.rs` | what `/proc` says about a process (to tell a session's process from a reused pid) |
| `src/desktop.rs` | notifications, other programs' windows (through niri's socket), and what folders open in |
| `src/appearance.rs`, `src/settings.rs` | fonts, the desktop's text size, the kept zoom |
| `src/ui/` | the Slint models kept in line with the state, and the callbacks |
| `ui/*.slint` | the window |
| `assets/` | the heron (standing, its idle poses, and three wing beats for its flight), the app's icon and its desktop entry |

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
refuse unknown keys, those in `src/external/` take them). `cargo clippy --all-targets -- -D
warnings`, `ast-grep scan`, `cargo deny check` (licenses that combine with the GPL, advisories,
sources) and `cargo test` are clean.

## Releasing

A release is a tag: `v` and the version in `Cargo.toml`, pushed, builds the packages
(`.github/workflows/release.yml`), installs and starts each on Debian, Ubuntu, Fedora and
AlmaLinux, and drafts the release for a person to publish.

