# edualc

A native desktop window for Claude Code, in Rust and Slint.

It doesn't reimplement Claude Code. It runs the `claude` CLI you already have
(`claude -p --input-format stream-json --output-format stream-json --permission-prompt-tool stdio`),
the same protocol the Agent SDKs use. Your login, settings, CLAUDE.md, skills, hooks, MCP servers and
sessions all work as they do in the terminal, and sessions started here open in the terminal and the
other way around.

## What it does

- **The conversation, as a document.** Markdown is rendered (headings, lists, code blocks with copy
  buttons, tables, quotes), text streams in as it's written, and every part of it can be selected and
  copied (Ctrl+C), including tool output.
- **Tool calls as cards, runs of them as one line.** Each card shows its command, file or query; edits
  show a diff; outputs are folded and copyable in full. Two or more calls in a row fold into one line
  ("Ran 1 command, read 2 files, edited main.rs") that opens on a click; while it works the line shows
  the current step, and a failed call shows in red. A call waiting for your permission is never folded
  away.
- **Permissions in the window.** Allow, deny, deny with what to do instead, or "always allow" with the
  rule the CLI suggests, named in full with where it is kept ("… in this project"). Claude's
  questions (AskUserQuestion) appear as choices, an option's preview (code, a mockup) beside them,
  with notes to send along with the answer. The buttons arm only 0.7 s after a request appears,
  and every answer names the request it was given for: a click can't answer a request it wasn't
  meant for. The tool card shows the input that is approved.
- **A sandbox for Claude Code.** The `claude` process, and everything it starts (commands, hooks, MCP
  servers), runs in a Landlock sandbox: the system and toolchains read-only; writable only the
  project, its git folder, caches, temporary files and Claude Code's own data; no private files, no
  socket but the ssh agent's (no D-Bus, no niri), no signals outside, no `sudo`. What Claude Code
  runs on its own (`settings.json`, hooks, skills, plugins) stays read-only. The header's chip says
  how far the kernel enforces it and turns it off, or lets the display in for UI tests. The usual
  toolchains are readable (cargo's config, not its credentials; nvm, pnpm, uv, mise, pyenv, bun) and
  Go, Maven and Gradle may write their homes; more is let in with `sandbox_read` and
  `sandbox_write` in `~/.config/edualc/settings.json`. The keyring stays out of reach, so a `gh`
  that keeps its token there isn't logged in inside. The network stays open (Landlock tells ports
  apart, not hosts). If your Claude Code settings turn on its own
  sandbox (bubblewrap, for its commands), that one is kept and this one stays off: the two don't
  stack.
- **Trust before running.** `claude -p` skips the terminal's trust dialog, and a folder's hooks and
  settings run as it starts; a folder neither the terminal nor this window trusted asks first.
- **Plan review with comments.** When Claude presents a plan, it opens part by part. Comment on any
  heading, paragraph or single list item, then:
  - **Keep planning**: Claude gets each comment quoted with the part it's about, and revises.
  - **Approve**: the comments go into the approved plan as notes under their parts. The CLI hands
    Claude an edited plan as the "Approved Plan (edited by user)".
- **Sessions.** This folder's past sessions are listed with their titles and ages; opening one shows its
  transcript and continues it. Any folder Claude Code was used in is one click away. Links open only
  if they are web pages or mail.
- **Pasted images.** Ctrl+V in the composer takes the clipboard's PNG, JPEG, GIF or WebP along
  with the next message, shown as a thumbnail until it is sent. A GIF or WebP goes as a PNG (an
  animation as its first frame). The image is never decoded in the window:
  the daemon reads the clipboard and hands the bytes to `edualc --decode-image`, a process that
  first confines itself (no file, socket or other process to reach, no executable memory, bounded
  memory and time, seccomp killing it on anything else) and answers with the image encoded again
  (its metadata gone), scaled to what the current models take (2,576 px on the long edge, 4,784
  visual tokens), and a thumbnail. A crafted image can take over only that process.
- **Images in the conversation.** The images you sent show where your messages are. The images a
  tool returned to Claude, such as a file it read or a screenshot, show in the tool's card, which
  opens for them. That way you see what Claude saw. They are decoded the same way, confined, by
  `edualc --show-image`, which the window starts itself, one image at a time. An image that can't
  be shown says why.
- **Sessions outlive their window.** Closing the window while a session works, or waits for you,
  leaves it running; opening it again (from the list, or with `--resume`) shows it where it is: its
  transcript, then what it asked meanwhile, asked again. An idle session ends with its window, as its
  transcript continues it anyway. One left alone and quiet for an hour ends.
- **Running sessions, everywhere.** Every Claude Code session running on the machine, in any
  terminal or folder, is listed with what it's doing. You get a desktop notification when one starts
  waiting for its user. Opening one follows it live without touching it. From there, **Go to its
  window** (niri) or **Continue here as a copy** (`--resume --fork-session`), since two processes
  shouldn't drive one session.
- **Side questions (`/btw`), beside the conversation.** `/btw …` (or the header's button) asks through
  the CLI's own `side_question`: answered with the conversation's context, without joining it or
  running tools. The answers wait in a side panel instead of taking over the input, which stays free.
  Ask follow-ups, copy an answer, put the questions and answers into the draft, or branch them into a new
  session (`btw: <question>`). Fold the panel away while something else needs you; the header's
  button brings it back and shows when an answer came meanwhile.
- **Shell commands (`!`), as in the terminal.** `! command` is run by Claude Code itself, through its
  undocumented `bash_command` input, so in the same sandbox as its own commands. It runs beside the
  conversation, as a card that shows its output; prompts can be sent meanwhile. The command and what it
  printed go to Claude with your next prompt, as the terminal gives them. One runs at a time, and Stop
  (Esc) stops it. Stop interrupts, so it stops Claude's turn too if one is running.
- **Worktrees.** When a session works in a git worktree (`claude --worktree`, the EnterWorktree
  tool), the header shows its name and branch, and the session lists mark it. **Open in** offers this
  window, Zed, VS Code, a terminal or the file manager, whichever are installed. Worktrees come from
  the transcript's `worktree-state` and from a session's working directory being a linked worktree.
- **The composer.** The bottom edge of the conversation is the text box, set apart by a line that
  takes the accent while writing. Under the text: the permission mode (coloured by how much it lets
  through) and the model, each with a menu that says what its choices mean; what Claude is doing,
  while it works; how full the context is; and one button that sends, or stops while Claude works.
  Slash commands complete with Tab; Esc interrupts. The box grows with its text; dragging its top
  edge gives it a height of your own (kept; a double click fits it to the text again), one button
  makes it taller over the conversation and another gives it the whole column, to see all of a long
  paste. The account, cost and plan limits sit at the foot of the sidebar, which Ctrl+B hides; a
  click on them opens the usage details over the conversation, which stays in sight around them.
  While Claude Code isn't sandboxed, the foot of the sidebar says so above them too.

## Look

Gruvbox, dark or light as the desktop asks, with a few colours lifted to WCAG AA contrast where
Gruvbox's fall short (`ui/theme.slint` says which and why). The sidebar's palette button switches to
a plain look instead: greys with a blue accent, also dark or light, at AA on every surface. The choice
is kept. Text is Fira Sans, code Fira Code
(fontconfig's sans and monospace where Fira isn't installed), at the size the desktop configures
(GNOME's interface font and text scaling, or KDE's), scaled for the screen. Ctrl+= and Ctrl+- zoom in
10% steps, Ctrl+0 resets; the zoom is kept in `~/.config/edualc/settings.json`. Controls
and rows grow with the text, and the sidebar and side panel give way first in narrow windows.

## Running

```sh
cargo run --release -- [--model MODEL] [--permission-mode MODE] [--resume SESSION] [DIR] [PROMPT]
```

`DIR` defaults to the current directory. `CLAUDE_BIN` names another `claude`, and `RUST_LOG=edualc=debug`
shows what the CLI writes to stderr. Slint draws the window itself (winit and femtovg), so there is no
Qt or GTK dependency.

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
| `src/watcher.rs` | inotify: noticing the running sessions' records and a followed transcript change |
| `src/sandbox.rs`, `src/trust.rs` | the Landlock sandbox for `claude`, and which folders are trusted |
| `src/process.rs` | what `/proc` says about a process (to tell a session's process from a reused pid) |
| `src/desktop.rs` | notifications, other programs' windows (through niri's socket), and what folders open in |
| `src/appearance.rs`, `src/settings.rs` | fonts, the desktop's text size, the kept zoom |
| `src/ui/` | the Slint models kept in line with the state, and the callbacks |
| `ui/*.slint` | the window |

The window doesn't run Claude Code itself: the first window starts `edualc --daemon`, which runs
the `claude` processes and does what the desktop is asked (opening editors, notifications,
stopping sessions elsewhere), and ends a few seconds after its last window and session. They
speak over a Unix socket in `$XDG_RUNTIME_DIR/edualc`: a folder of the user's alone, checked
through its descriptor and never through a link; a lock deciding which process is the daemon,
so no two bind and none unbinds another's; peers checked to be this user running this program;
messages framed by postcard's COBS, read into a fixed buffer that ends the connection when a
frame doesn't fit. The daemon's parts are
actors in the manner of "More Actors with Tokio" (each its data, run by an event loop it is
given its channels for, channels without cycles, shutting down as they close), with sans-io
logic tested without sockets or processes.

The window confines itself before it makes any thread: it reads the system and the user's
files but their secrets (keys, keyrings, browser profiles, credentials), writes only its
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

## Known limits

- Slint is taken from an unreleased commit (2026-10-31): selectable `StyledText` (slint-ui/slint#13985)
  is merged but not yet released. Go back to crates.io once 1.19 is out.
- Side questions need a session running here; for one watched from another window, continue it here
  as a copy first.
- `@` file mentions and per-session effort aren't there yet. Images are pasted, not dropped in.
  An image shows at most 480 px wide; there is no larger view yet.
- Finding another session's window needs niri.

## License

GPL-3.0-or-later (`LICENSE`). Slint is used under its GPL-3.0-only option, so a built edualc is
distributed under GPL-3.0.
