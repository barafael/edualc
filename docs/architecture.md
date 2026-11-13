# How edualc is built

## The window and the daemon

The window doesn't run Claude Code itself. The first window you open starts a daemon,
`edualc --daemon`, and later windows connect to it. The daemon:

- runs the `claude` processes, or agy's adapter, for every window;
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
- **Every peer must be you, running the same program,** checked with the kernel's peer credentials
  and the program's path.
- **Messages are framed with postcard's COBS** and read into a fixed buffer. A frame that doesn't
  fit ends the connection.

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
- no network listener;
- images, decoded in a process of their own that locks itself down before it reads them (`decode`).

## Doing nothing while nothing happens

There is no animation loop. The window learns that files other processes write have changed
through inotify, and checks nothing on a schedule, with two exceptions:

- once a minute, the window checks the running sessions, to catch one that crashed and left its
  record behind;
- while you follow a background command in the side panel, its output is fetched every second.

`SLINT_DEBUG_PERFORMANCE=refresh_lazy,console` prints every frame drawn.

## Layout

| | |
|---|---|
| `src/main.rs`, `src/start.rs`, `src/start/manual.rs` | how the program starts: its command line, reaching or starting the daemon, opening the window; the manual page |
| `src/daemon.rs`, `src/spawn.rs` | the daemon: running `claude` or agy's adapter, and acting on the desktop |
| `src/claude.rs` | a session's process as the window sees it: what it is started with, what its output means, and the handle that writes to it |
| `src/agy.rs`, `src/agy/` | an agy session: the adapter (`edualc --agy-adapter`) that speaks the Agent Client Protocol to `agy acp` and Claude Code's stream-json to the daemon; agy's data folder per project; its inbox; the messages it sends to other sessions |
| `src/ipc.rs`, `src/ipc/socket.rs`, `src/link.rs` | the messages between the window and the daemon, the daemon's socket, and the window's connection to it |
| `src/decode.rs`, `src/decode/helper.rs` | making images safe to show and send, and the locked-down process that decodes them |
| `src/external/` | formats other programs write, as types: Claude Code's stream-json, transcripts and files, the Agent Client Protocol, and niri's IPC. These accept keys they don't know, since each release adds some. |
| `src/app.rs` | `App`, the single owner of all state. UI callbacks, process output, timers and background work reach it as `Event`s on one channel. |
| `src/app/protocol.rs` | applying what the process says to the state |
| `src/app/side.rs`, `src/app/peek.rs` | the side panel: side questions (`/btw`), and following a subagent or background command |
| `src/app/tasks.rs` | what runs beside the turn: background tasks and subagents |
| `src/app/usage.rs` | the usage panel (`/usage`, `/context`) |
| `src/app/images.rs` | the conversation's images, decoded for showing |
| `src/app/worktree.rs` | a session's worktree, and opening it |
| `src/chat.rs` | the conversation as entries, live or from a transcript |
| `src/tools.rs`, `src/markdown.rs`, `src/plan.rs` | how tool calls, Markdown and plan comments are shown |
| `src/highlight.rs`, `src/colouring.rs` | colouring code by its language, on a thread of its own |
| `src/grouping.rs` | folding runs of tool calls into one line |
| `src/sessions.rs` | saved sessions (`~/.claude/projects`), running ones (`~/.claude/sessions`), and their names |
| `src/sidebar.rs` | the sidebar's list: running sessions, then this folder's earlier ones |
| `src/watcher.rs` | inotify: noticing changes to the running sessions' records and a followed transcript |
| `src/trust.rs` | which folders are trusted, and what an untrusted folder would run |
| `src/folder.rs` | checking a folder you name before the window switches to it |
| `src/process.rs` | what `/proc` says about a process, to tell a session's process from a reused process ID |
| `src/desktop.rs` | notifications, other programs' windows (through niri's socket), and what a folder opens in |
| `src/appearance.rs`, `src/cjk.rs` | fonts and the desktop's text size; which language a text's Han characters are in |
| `src/settings.rs` | the settings you keep between runs |
| `src/words.rs` | text that isn't a closed set of ours: words people read, or someone else's |
| `src/ui/` | keeping the Slint models in line with the state, and the callbacks |
| `ui/*.slint` | the window |
| `src/web/`, `web/` | the browser demo: a scripted daemon, the demo project's sessions, and the page with its fonts |
| `assets/` | the heron (standing, its idle poses, and its wing beats), the app's icon, desktop entry, metainfo and manual page |
| `dist/` | packaging: one description in Nickel (`package.ncl`) that the deb, rpm and Arch files are written from |

## Conventions

- **No shared mutable state.** `App` owns the state, and everything else reaches it by message.
  `clippy.toml` refuses cells, `Rc`, `Arc` and locks.
- **Errors are typed,** and nothing is silently discarded.
- **Lints:** the lint set in `Cargo.toml` and the ast-grep rules in `lint/`. Formats edualc owns
  refuse unknown keys; those in `src/external/` accept them.
- **Checks:** `cargo clippy --all-targets -- -D warnings`, `ast-grep scan`, `cargo deny check`
  (licences that combine with the GPL, advisories, sources) and `cargo test` all pass. CI runs
  them on every push to main and on every pull request.

## The browser demo

The same window builds for `wasm32-unknown-unknown` and runs in a web page.
`.github/workflows/pages.yml` publishes it to GitHub Pages.

The browser has no daemon, sockets or processes, so the build leaves them out
(`#[cfg(not(target_arch = "wasm32"))]`). The few parts of the window that use them have browser
versions in `src/web/`, with the same interfaces:

- `link` leads to a script in the page (`src/web/daemon.rs`) that answers as `claude -p` would.
  A word in your prompt picks the scenario it plays (`src/web/scenarios.rs`).
- `watcher` never fires.
- Images are decoded where they're shown, since the page is the sandbox.
- Background work is done at once, since a page has no threads.
- `sessions` reads the demo project's transcripts (`web/demo/`, as Claude Code writes them)
  instead of `~/.claude`.

Everything else is the code that runs on Linux: `App`, the conversation, Markdown, tool cards,
plan review and the view.

`cargo clippy --target wasm32-unknown-unknown -- -D warnings -A dead_code` checks this build, and
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
