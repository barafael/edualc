# edualc

<img src="assets/io.github.felintusun.edualc.svg" alt="" width="72" align="right">

**A native Linux desktop window for Claude Code.**

edualc runs the `claude` CLI you already have, so your sign-in, settings, CLAUDE.md, skills, hooks,
MCP servers and sessions work as they do in the terminal, and a session moves freely between the
two. What it adds is a window made for reading and steering the work.

![A conversation: Claude explains why GPS noise inflates the elevation gain, its edits and commands folded into one line, and the result in a table](docs/screenshots/fixed.webp)

**[Try it in your browser](https://felintusun.github.io/edualc/)**: the window itself, compiled to
WebAssembly, on a demo project. Claude is played by a script there, so nothing you type leaves
the page.

## What it does differently

- **Trust before anything runs.** A folder you haven't trusted yet shows what its settings would
  run (hooks, MCP servers, environment) before Claude Code starts there.
- **A conversation you can read.** Markdown is rendered, and everything can be selected and copied.
  Tool calls are cards with diffs, and a run of them folds into one line.
- **Permissions you can't misclick.** Allow, deny with what to do instead, or always allow with the
  exact rule named. The buttons arm a moment after a request appears, and each answer is bound to
  the request it was given for.
- **Plans you review like a document.** Comment on any heading, paragraph or list item, then send
  Claude back to planning or approve with your notes attached.
- **Every session on the machine in one list.** Sessions running in any terminal or folder, with
  what each is doing, and a notification when one needs you. A session keeps running when its
  window closes, and keeps a name other sessions can message it by.
- **Side questions on the side.** `/btw` answers in a panel next to the conversation, which goes on
  without it.
- **A native app.** Rust and Slint, no browser engine, and no polling while nothing happens. Every
  control is reachable by keyboard, screen readers can read the window, and it follows the
  desktop's light or dark setting and text size.

All of it in detail: [docs/features.md](docs/features.md).

## Screenshots

| | |
|---|---|
| ![An edit waiting for permission, its diff shown in full](docs/screenshots/edit-permission.webp) | ![A plan under review, with comments on two of its steps](docs/screenshots/plan-review.webp) |
| ![Claude's questions as choices, with a code preview beside the selected one](docs/screenshots/questions.webp) | ![A side question answered in a panel beside the conversation](docs/screenshots/side-question.webp) |
| ![A cloned folder's hook, environment and MCP server, listed before anything runs](docs/screenshots/trust.webp) | ![The same conversation in light Gruvbox](docs/screenshots/light.webp) |

## Install

You need Linux on Wayland, and Claude Code installed and signed in (`claude` on the PATH).

There is no release yet. Until the first, Arch users can build main with `makepkg -si` in
[`dist/aur/edualc-git`](dist/aur/edualc-git). Each [release](https://github.com/felintusun/edualc/releases)
will have packages for x86_64 and aarch64:

- **Debian 12+, Ubuntu 24.04+:** `sudo apt install ./edualc_*.deb`
- **Fedora, AlmaLinux 10:** `sudo dnf install ./edualc-*.rpm`
- **Arch:** `makepkg -si` in [`dist/aur/edualc`](dist/aur/edualc)
- **Anything else:** the tarball has the binary, its desktop entry and its icon.

`wl-clipboard` (for pasting images) is recommended.

## Use

```sh
edualc [DIR] [PROMPT]
edualc --resume SESSION
```

`edualc --help` lists the rest. Settings live in `~/.config/edualc/settings.json`.

Claude Code runs as it does in a terminal: edualc no longer puts a sandbox around it. Sessions
that ran in that sandbox kept their memory, plans, todos and file history in
`~/.local/state/edualc/claude/<project>/`, which edualc leaves where it is; their transcripts
are in `~/.claude` already.

## Known limits

- Finding another session's window needs niri.
- `@` file mentions aren't there yet, and images are pasted, not dropped in.
- Slint comes from a fork at an unreleased commit, until the features edualc needs are released.

## Building

`cargo build --release`. How the code is laid out, and how a release is made, is in
[docs/architecture.md](docs/architecture.md).

## License

GPL-3.0-or-later ([LICENSE](LICENSE)). Slint is used under its GPL-3.0-only option, so a built
edualc is distributed under GPL-3.0.
