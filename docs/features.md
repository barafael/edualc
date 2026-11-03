# What edualc does, in full

The README has the short version; this is every feature, as the window has it.

## Features

- **The conversation, as a document.** Markdown is rendered (headings, lists, code blocks with copy
  buttons, tables, quotes), text streams in as it's written, and every part of it can be selected and
  copied (Ctrl+C), including tool output. Code blocks are coloured by their language (bat's
  grammars, in the look's colours), a line at a time as they stream in.
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
  how far the kernel enforces it and turns it off, lets the display in for UI tests, or, for the
  folder shown, USB devices and serial ports for debug probes and boards: every USB device your
  account may open, a phone too, and the ports there as it starts (one plugged in again needs it
  started again); HID probes stay out, as hidraw is keyboards' and security keys' too. The usual
  toolchains are readable (cargo's config, not its credentials; nvm, pnpm, uv, mise, pyenv, bun) and
  Go, Maven and Gradle may write their homes; more is let in with `sandbox_read` and
  `sandbox_write` in `~/.config/edualc/settings.json`. The keyring stays out of reach, so a `gh`
  that keeps its token there isn't logged in inside. The network stays open (Landlock tells ports
  apart, not hosts). If your Claude Code settings turn on its own
  sandbox (bubblewrap, for its commands), that one is kept and this one stays off: the two don't
  stack.
- **Trust before running.** `claude -p` skips the terminal's trust dialog, and a folder's hooks and
  settings run as it starts; a folder neither the terminal nor this window trusted asks first,
  listing what its `.claude/settings.json`, `settings.local.json` and `.mcp.json` would run or
  set: hooks with their commands, MCP servers with their command lines, helpers, plugins, and
  environment variables by name. The daemon reads them without following links, and says when a
  file can't be read rather than leaving it out.
- **Plan review with comments.** When Claude presents a plan, it opens part by part. Comment on any
  heading, paragraph or single list item, then:
  - **Keep planning**: Claude gets each comment quoted with the part it's about, and revises.
  - **Approve**: the comments go into the approved plan as notes under their parts. The CLI hands
    Claude an edited plan as the "Approved Plan (edited by user)".
- **Sessions, running and earlier, in one list.** The sidebar lists first the sessions running now,
  in any folder and this window's own among them, under "Running · N" (with how many need you),
  then this folder's earlier ones. A running session's row has two lines: its title, then what it
  does (needs you, working, idle), the name to message it by and its folder when another; its
  running subagents are counted beside it. Needing you shows a hand, working a dot, idle a ring, so
  the states aren't told by colour alone; a session that needs you comes first, the others keep
  their place. Opening an earlier session shows its transcript and continues it. A cut-off title
  shows whole on hover, and the sidebar's right edge is dragged to widen it (kept; a double click
  gives the default back) or moved with the arrow keys. Any folder Claude Code was used in is one
  click away. Links open only if they are web pages or mail.
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
- **A name to message a session by.** Other Claude Code sessions message a session by its name,
  which Claude Code takes from the process id, so it changes whenever the session is started again.
  edualc names each session after its id and its project instead, for example
  `proud-moth-claude-but-sane-27`: an adjective and a noun from the `petname` word lists, the
  folder's name and two digits, all but the folder picked by the id, so the same however often
  the session is started. It picks a new session's id itself (`--session-id`), so the name holds from the first
  start.
- **Running sessions, everywhere.** Every Claude Code session running on the machine, in any
  terminal or folder, is in that list with what it's doing. You get a desktop notification when one
  starts waiting for its user. Opening one follows it live without touching it (or shows it here,
  if this window's daemon runs it). From there, **Go to its window** (niri) or **Continue here as a
  copy** (`--resume --fork-session`), since two processes shouldn't drive one session.
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
  (Esc) stops it. Stop interrupts, so it stops Claude's turn too if one is running. While the draft
  starts with `!` (or the full-width `！` a Japanese or Chinese input method types), the composer
  shows it as a command, in monospace, and says what Enter will do. The `>_` button puts the `!` in
  front or takes it away. A draft starting with a Markdown image (`![…](…)`) stays a message.
- **Worktrees.** When a session works in a git worktree (`claude --worktree`, the EnterWorktree
  tool), the header shows its name and branch, and the session lists mark it. **Open in** offers this
  window, Zed, VS Code, a terminal or the file manager, whichever are installed. Worktrees come from
  the transcript's `worktree-state` and from a session's working directory being a linked worktree.
- **The composer.** The bottom edge of the conversation is the text box, set apart by a line that
  takes the accent while writing. Under the text: the permission mode (coloured by how much it lets
  through) and the model, each with a menu that says what its choices mean; what Claude is doing,
  while it works (a request Claude Code tries again, "Retrying 3/10 · the API is overloaded", or
  "No answer for 41s" once it has said nothing for half a minute while an answer is due); how
  full the context is; and one button that sends, or stops while Claude works.
  Slash commands matching what is typed are listed: Up and Down choose one, Enter runs it, and →
  or Tab put it in the draft, with what it takes hinted after it (`[instructions]`) until
  something follows or Esc dismisses the hint. Down on the last line goes to the end of the
  draft; Esc with nothing written interrupts Claude. The box grows with its text; dragging its top
  edge gives it a height of your own (kept; a double click fits it to the text again), one button
  makes it taller over the conversation and another gives it the whole column, to see all of a long
  paste. The account, cost and plan limits sit at the foot of the sidebar, which Ctrl+B hides; a
  click on them opens the usage details over the conversation, which stays in sight around them.
  While Claude Code isn't sandboxed, the foot of the sidebar says so above them too.
- **The keyboard.** Every control can be reached with Tab and pressed with Enter or Space, a ring
  showing where the focus is. Tab and Shift+Tab stay within the area they are in: the sidebar, the
  conversation, what Claude asks together with the composer, or the side panel. F6 and Shift+F6 go
  from one area to the next, and Ctrl+K goes to the sessions' filter. In the conversation the
  arrow keys, Page Up and Down, Home and End scroll. When Claude asks, the focus comes into the
  question (never onto Allow), unless you are writing a draft: Up and Down go through the options,
  Space picks one, Enter picks one and goes on to the next question, Left and Right go between
  the questions.
- **Settings.** The gear in the sidebar (or Ctrl+,) opens them over the conversation; each takes
  effect at once and is kept in `settings.json`. The key that sends is one: Enter, or Ctrl+Enter
  for those who'd rather Enter started a new line, as many chat apps let one choose.

## Look

Gruvbox, dark or light as the desktop asks, with a few colours lifted to WCAG AA contrast where
Gruvbox's fall short (`ui/theme.slint` says which and why). The sidebar's palette button switches to
a plain look instead: greys with a blue accent, also dark or light, at AA on every surface. The choice
is kept. Text is Fira Sans, code Fira Code
(fontconfig's sans and monospace where Fira isn't installed), at the size the desktop configures
(GNOME's interface font and text scaling, or KDE's), scaled for the screen. Ctrl+= and Ctrl+- zoom in
10% steps, Ctrl+0 resets; the zoom is kept in `~/.config/edualc/settings.json`. Controls
and rows grow with the text, and the sidebar and side panel give way first in narrow windows.

edualc is a heron (`assets/heron.svg`): it stands at the top of the sidebar and greets a new
session, flies off when the conversation begins and flies in when a new session opens
(`ui/heron.slint`, with the wing beats `assets/heron-wings-*.svg`). About once a minute the one
in the sidebar looks over its shoulder, preens or flaps its wings, in turn, for a second or so
(`assets/heron-looking-back.svg`, `heron-preening-*.svg`, `heron-flapping-*.svg`). It is also
the app's icon (`assets/edualc.svg`, on Gruvbox's light background).

