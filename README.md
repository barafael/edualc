# edualc

<img src="assets/io.github.felintusun.edualc.svg" alt="" width="72" align="right">

**A native Linux desktop window for Claude Code.**

edualc runs the `claude` program you already have. Your sign-in, settings, CLAUDE.md, skills,
hooks, MCP servers and sessions all work as they do in the terminal, and you can move a session
between the terminal and the window at any time. The window is made for reading Claude's work and
steering it.

![A conversation: Claude explains why GPS noise inflates the elevation gain, its edits and commands folded into one line, and the result in a table](docs/screenshots/fixed.webp)

**[Try it in your browser](https://felintusun.github.io/edualc/)**: the real window, compiled to
WebAssembly, with a demo project. A script plays Claude, so nothing you type leaves the page.

**And on Android.** The same window again, as an app that reaches edualcd over the network (see
[In a browser, or from another machine](#in-a-browser-or-from-another-machine)).

## What you get

- **You see what a folder would run before it runs.** If you haven't trusted a folder yet, the
  window lists its hooks, MCP servers and environment variables before Claude Code starts there.
- **A conversation you can read.** Markdown is rendered, and you can select and copy all of it.
  Tool calls show as cards with diffs, and a run of them folds into one line.
- **Permission buttons you can't misclick.** Allow, deny and say what to do instead, or always
  allow with the exact rule shown. The buttons respond only a moment after a request appears, and
  each answer goes to the request it was meant for.
- **Plans you review like a document.** Comment on any heading, paragraph or list item. Then send
  Claude back to planning, or approve the plan with your comments attached.
- **All your sessions in one list.** The sidebar shows the sessions running in any terminal or
  folder and what each is doing, and you get a notification when one needs you. A session that is
  working or waiting for you keeps running when you close its window.
- **Side questions on the side.** `/btw` answers in a panel next to the conversation, which carries
  on meanwhile.
- **A native app.** It's built with Rust and Slint, with no browser engine, and it does almost
  nothing while nothing changes. You can reach every control with the keyboard, screen readers can
  read the window, and it follows your desktop's light or dark setting and text size.

Every feature in detail: [docs/features.md](docs/features.md).

## Screenshots

| | |
|---|---|
| ![An edit waiting for permission, its diff shown in full](docs/screenshots/edit-permission.webp) | ![A plan under review, with comments on two of its steps](docs/screenshots/plan-review.webp) |
| ![Claude's questions as choices, with a code preview beside the selected one](docs/screenshots/questions.webp) | ![A side question answered in a panel beside the conversation](docs/screenshots/side-question.webp) |
| ![A cloned folder's hook, environment and MCP server, listed before anything runs](docs/screenshots/trust.webp) | ![The same conversation in light Gruvbox](docs/screenshots/light.webp) |

## Install

You need Linux with Wayland, and Claude Code installed and signed in (`claude` on your PATH).

There is no release yet. Until there is, build the main branch: on Arch with `makepkg -si` in
[`dist/aur/edualc-git`](dist/aur/edualc-git), anywhere else as described under
[Building](#building).

Releases will have packages for x86_64 and aarch64:

- **Debian 12+, Ubuntu 24.04+:** `sudo apt install ./edualc_*.deb`
- **Fedora, AlmaLinux 10:** `sudo dnf install ./edualc-*.rpm`
- **Arch:** `makepkg -si` in [`dist/aur/edualc`](dist/aur/edualc)
- **Other distributions:** the tarball has the program, its desktop entry and its icon.

To paste images, install `wl-clipboard` too.

## Use

```sh
edualc [DIR] [PROMPT]
edualc --resume SESSION
edualc --backend agy DIR
```

`edualc --help` lists every option. Your settings are in `~/.config/edualc/settings.json`.

`--backend agy` runs Google's Antigravity CLI instead of Claude Code. It needs `agy` on your PATH.

**Coming from an earlier version?** Earlier versions ran Claude Code in a sandbox. Now Claude Code
runs exactly as it does in your terminal. Sessions that ran in the sandbox kept their memory,
plans, todos and file history in `~/.local/state/edualc/claude/<project>/`. edualc leaves that
folder alone, so you can copy anything you still need from it. Their transcripts are already in
`~/.claude`.

## Known limits

- Jumping to another session's window works only on niri.
- You can't mention files with `@` yet, and you paste images rather than drag them in.
- You can choose agy only on the command line, not in the window.
- edualc builds Slint from a fork, until the features it needs are in a Slint release.
- The Android app keeps its connection only while it's on screen. Android stops an app in the
  background, and the app reaches edualcd again when it comes back, so you hear of a session
  waiting for you only then. It draws images on the window's thread, which many large ones slow
  down. There is no release of it yet: build it as described below.

## In a browser, or from another machine

edualcd, the daemon the window starts, can also serve windows that aren't on this machine, and a
web page that is the same window:

1. Run `systemctl --user enable --now edualcd`. It listens on the loopback address.
2. Run `edualcd --pair`. It prints an address that opens the page, or pairs a window or an app.
   The token is in the part of the address that is never sent to a server.

To reach edualcd from other machines, see the comments in `edualcd.service`. `edualcd --demo`
serves the page to anyone, with a script in place of Claude Code, on a demo project.

**On Android,** the app lists the edualcds it's paired with, and pairs with another by the
address `edualcd --pair` prints. When it starts, it reaches the one it reached last, and opens the
session it showed last there. A demo's address (`http://host:port/`) works too, and the window
says its connection isn't encrypted. "Choose another edualcd" in the settings goes back to the
list. The app keeps its pairings out of Android's backups. Links open in Android's browser,
notifications are Android's (it asks you the first time the window opens), and the composer's
image button pastes the clipboard's image. On the screen's keyboard, Enter starts a new line and
the send button sends; on a keyboard with keys, Enter sends.

## Building

Run `cargo build --release` to build the window and the daemon, without its page. `just
release-build` also builds edualcd with its page; it needs
[dioxus-cli](https://dioxuslabs.com) (`cargo install --locked dioxus-cli`).
[docs/architecture.md](docs/architecture.md) explains how the code is laid out and how a release is
made.

dx builds the Android app too. It needs the Android SDK and NDK (`ANDROID_HOME`,
`ANDROID_NDK_HOME`), a JDK 17 or later as `JAVA_HOME`, and the Rust target: run `dx build --android
-p edualc-server --features mobile --target aarch64-linux-android` (for the emulator,
`x86_64-linux-android`). `dx serve` with the same arguments installs and starts it on a running
emulator or a connected phone. In the emulator, the machine it runs on is `10.0.2.2`, so
`edualcd --demo` there is `http://10.0.2.2:7878/`.

The Android app is built by dx too, with the Android SDK and NDK (`ANDROID_HOME`,
`ANDROID_NDK_HOME`, a JDK 17 or later as `JAVA_HOME`) and the Rust target:
`dx build --android -p edualc-server --features mobile --target aarch64-linux-android` (for the
emulator, `x86_64-linux-android`); `dx serve` with the same arguments installs and starts it on
a running emulator or a connected phone. In the emulator, the machine it runs on is `10.0.2.2`:
`edualcd --demo` there is `http://10.0.2.2:7878/`.

## License

GPL-3.0-or-later ([LICENSE](LICENSE)). edualc uses Slint under its GPL-3.0-only option, so a
built edualc is distributed under GPL-3.0.
