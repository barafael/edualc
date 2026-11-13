# Every feature in detail

The README has the short version. This page describes everything the window does.

## Claude Code, as in your terminal

edualc starts the same `claude` program you run in a terminal, with your rights, your
`~/.claude` and `~/.claude.json`, and your settings. Claude Code's own sandbox for its commands
(bubblewrap) and Remote Control work as they do there. edualc adds no sandbox of its own.

edualc keeps its own parts private. Only you can read its daemon's socket and its state files,
and it doesn't listen on the network. Images are decoded in a separate, locked-down
process (see [Images](#images)).

## Trust before anything runs

A folder's hooks and settings take effect the moment Claude Code starts there. In a terminal,
Claude Code asks first whether you trust the folder, but the mode edualc runs it in (`claude -p`)
skips that question. So the window asks instead, for any folder that neither you in a terminal
nor this window has trusted.

It lists what the folder's `.claude/settings.json`, `.claude/settings.local.json` and `.mcp.json`
would run or set:

- hooks, with their commands;
- MCP servers, with their command lines;
- helpers and plugins;
- environment variables, by name.

The files are read without following links. If a file can't be read, the window says so rather
than leaving it out.

## The conversation

- **Markdown is rendered:** headings, lists, tables, quotes, and code blocks with a copy button.
  Text appears as Claude writes it.
- **Code is coloured** by its language, using bat's grammars in the window's own colours, a line at
  a time as it streams in, on a thread of its own: a long session opens, and the colours change,
  without the window waiting.
- **You can select and copy everything** with Ctrl+C, tool output included.

## Tool calls

- **Each call is a card** showing its command, file or query. An edit shows a diff. Output is
  folded, and you can copy it in full.
- **Two or more calls in a row fold into one line**, such as "Ran 1 command, read 2 files, edited
  main.rs". Click it to open the cards. While the calls run, the line shows the current step, and a
  failed call shows in red.
- **A call waiting for your permission is never folded away.**

## Permissions and questions

- **Answering a request:** allow, deny, deny and say what to do instead, or "always allow". "Always
  allow" names the exact rule Claude Code suggests and where it is kept, for example "… in this
  project".
- **Guarding against misclicks:** the buttons respond only 0.7 seconds after a request appears.
  Every answer names the request it was given for, so a click can't answer a different request.
  The tool card shows exactly the input you approve.
- **Claude's questions (AskUserQuestion)** appear as choices. An option's preview, such as code or
  a mockup, shows beside them, and you can add notes to your answer.

## Plans

When Claude presents a plan, it opens section by section. You can comment on any heading,
paragraph or single list item. Then choose:

- **Keep planning:** Claude gets each comment, quoted with the part it's about, and revises the
  plan.
- **Approve:** your comments go into the approved plan as notes under their parts. Claude receives
  it as an "Approved Plan (edited by user)".

## Sessions

### The list

The sidebar lists, first, the sessions running now, in any folder and in any terminal, under
"Running · N" (with how many need you). Below them are this folder's earlier sessions.

A running session's row shows its title, then its state (needs you, working or idle), the name to
message it by, and its folder if that's a different one. The number of its running subagents
appears beside it.

- **States aren't told by colour alone:** a hand means it needs you, a dot that it's working, a ring
  that it's idle.
- **A session that needs you moves to the top.** The others keep their place.
- **Opening an earlier session** shows its transcript and continues it.
- **A cut-off title** shows in full when you hover over it.
- **To widen the sidebar,** drag its right edge or use the arrow keys. The width is kept, and a
  double click restores the default. Ctrl+B hides the sidebar.
- **Any folder you've used Claude Code in** is one click away.

### Sessions running elsewhere

You get a desktop notification when any session on the machine starts waiting for you. Opening a
session that runs elsewhere, such as in a terminal, follows it live without touching it. From
there you can choose:

- **Go to its window:** focuses the terminal it runs in (niri only).
- **Continue here as a copy:** starts a copy in this window (`--resume --fork-session`), since two
  programs shouldn't drive one session.

### Sessions outlive their window

If you close the window while a session is working or waiting for you, the session keeps running.
When you open it again, from the list or with `--resume`, you see where it is: its transcript, then
anything it asked meanwhile, asked again. An idle session ends when you close its window, since its
transcript can continue it anyway. A session that isn't open in any window ends after an hour
without activity.

### A name that stays the same

Other Claude Code sessions message a session by its name. Claude Code makes that name from the
process ID, so it changes every time the session restarts. edualc gives each session it starts a
name made from the session's ID and its folder instead, for example
`proud-moth-claude-but-sane-27`: an adjective and a noun from the `petname` word lists, the
folder's name, and two digits. Since the ID picks the words and digits, the name stays the same
however often the session restarts. edualc picks a new session's ID itself (`--session-id`), so
the name holds from the first start.

## What runs beside the turn

A chip shows what Claude runs besides the current step, such as "2 agents, 1 command". Click it
for the list. You can stop each item, or all of them at once. If you stop the turn, these keep
running until you stop them here.

Click an item to follow it in the side panel. For a subagent, the panel shows what it was asked,
what it says, and each tool it calls. For a background command, it shows the end of the output.

## Side questions (`/btw`)

Type `/btw …`, or use the header's button, to ask a side question. Claude answers with the
conversation's context, without adding to the conversation or running tools.

Answers wait in a side panel, so the composer stays free. You can ask follow-ups, copy an answer,
put questions and answers into your draft, or branch them into a new session (titled
`btw: <question>`). You can fold the panel away; the header's button brings it back and shows when
an answer arrived meanwhile.

## Shell commands (`!`)

Start a message with `!` to run a shell command, as in the terminal. Claude Code runs it itself,
just like its own commands. The command shows as a card with its output, beside the conversation,
and you can keep sending prompts meanwhile. The command and its output go to Claude with your next
prompt, as in the terminal.

- One command runs at a time. Stop (Esc) stops it, and it stops Claude's turn too if one is
  running.
- While your draft starts with `!`, or the full-width `！` that Japanese and Chinese input methods
  type, the composer shows it in monospace and says what Enter will do.
- The `>_` button adds or removes the `!`.
- A draft that starts with a Markdown image (`![…](…)`) stays a normal message.

## Images

- **Pasting:** Ctrl+V in the composer attaches the clipboard's PNG, JPEG, GIF or WebP to your next
  message, shown as a thumbnail until you send it. A GIF or WebP is sent as a PNG; an animation
  as its first frame.
- **Scaling:** images are scaled to what the current models take: 2,576 px on the long edge, at most
  4,784 visual tokens. Their metadata is removed.
- **In the conversation:** images you sent show in your messages. Images a tool returned to Claude,
  such as a file it read or a screenshot, show in that tool's card, so you see what Claude saw. An
  image that can't be shown says why.
- **Decoding:** the window never decodes an image itself. A separate process does
  (`edualc --decode-image` for pasting, `edualc --show-image` for showing). Before it reads a byte,
  that process locks itself down: it can reach no file, socket or other process, it can't run new
  code, and its memory and time are limited. A crafted image can take over only that process.

## Worktrees

When a session works in a git worktree (`claude --worktree`, or the EnterWorktree tool), the header
shows the worktree's name and branch, and the session lists mark it. **Open in** offers this
window, Zed, VS Code, a terminal or the file manager, whichever you have installed.

## The composer

The text box sits at the bottom of the conversation. Its top line takes the accent colour while you
write.

Under the text you find:

- **The permission mode,** coloured by how much it lets through, and **the model.** Each has a menu
  that explains its choices.
- **What Claude is doing** while it works, such as "Retrying 3/10 · the API is overloaded". If an
  answer is due and Claude has said nothing for 30 seconds, it shows how long, such as "No answer
  for 41s".
- **How full the context is.**
- **One button** that sends, or stops while Claude works.

Typing:

- **Slash commands** that match what you type are listed. Up and Down choose one, Enter runs it,
  and → or Tab put it in your draft. A hint after the command shows what it takes, such as
  `[instructions]`, until you type more or press Esc.
- **Up** on the first line brings back your previous prompt in this session, and again for the one
  before. **Down** on the last line steps forward, and past the newest gives back what you were
  writing. Editing a recalled prompt makes it your draft.
- **Esc** with nothing written interrupts Claude.

Size:

- The box grows with its text. Drag its top edge to set your own height; it's kept, and a double
  click fits the box to the text again.
- One button makes it taller over the conversation, and another gives it the whole column, for
  long pastes.

## Usage

The sidebar's foot shows your account, cost and plan limits. Click them to see the usage details
(Claude Code's `/usage` and `/context`) over the conversation.

## Signing in

Claude Code's `/login` doesn't work in the mode edualc runs it in, although Claude Code suggests
it. When a turn fails because you aren't signed in, or when you type `/login`, the window shows
`claude auth login` to run in a terminal. "Try again" restarts Claude Code and sends the failed
message again.

## The keyboard

- **Every control** can be reached with Tab and pressed with Enter or Space. A ring shows where the
  focus is.
- **Tab and Shift+Tab** stay within the area you're in: the sidebar, the conversation, Claude's
  question together with the composer, or the side panel.
- **F6 and Shift+F6** move from one area to the next. **Ctrl+K** jumps to the sessions' filter.
- **In the conversation,** the arrow keys, Page Up, Page Down, Home and End scroll.
- **When Claude asks something,** the focus moves into the question, never onto Allow, unless
  you're writing a draft. Up and Down move through the options, Space picks one, Enter picks one
  and goes to the next question, and Left and Right move between questions.

## Settings

The gear in the sidebar, or Ctrl+,, opens the settings over the conversation. Each change takes
effect at once and is kept in `~/.config/edualc/settings.json`.

- **Sending:** Enter sends, or Ctrl+Enter does, if you'd rather Enter started a new line.
- **Text in Han characters alone:** Chinese, Japanese and Korean draw many Han characters
  differently. A message with kana is set as Japanese and one with Hangul as Korean. Here you
  choose how a message in Han characters alone is set.
- **Shut down edualc's daemon:** the daemon runs the sessions of every window, and a newly
  installed edualc takes over only after it shuts down. Shutting it down ends every session in
  every window; you can continue them from the sidebar in the next window you open.

## agy (Google's Antigravity CLI)

`edualc --backend agy DIR` runs agy instead of Claude Code. The window shows an agy session like
any other: streamed text and thinking, tool cards, permission requests, and the model menu from
`agy models`.

- **Messaging:** Claude Code sessions can message an agy session by its name. An agy session can
  message other sessions too; agy itself asks your permission before it sends.
- **Continuing:** agy sessions are listed in the sidebar and continue like Claude Code's. Their
  transcripts are kept in `~/.local/state/edualc/agy/`.
- **Not available with agy:** shell commands (`!`) and the trust question.

## Look

The window uses Gruvbox, dark or light as your desktop asks. A few colours are lifted to WCAG AA
contrast where Gruvbox's fall short; `ui/theme.slint` says which and why. The sidebar's palette
button switches to a plain look instead: greys with a blue accent, also dark or light, at AA
contrast on every surface. Your choice is kept.

Text is set in Fira Sans and code in Fira Code. Where Fira isn't installed, your desktop's sans and
monospace fonts are used. Text has the size your desktop sets (GNOME's interface font and text
scaling, or KDE's), scaled for the screen.

- **Ctrl+=** and **Ctrl+-** zoom in 10% steps, and **Ctrl+0** resets. The zoom is kept.
- Controls and rows grow with the text. In a narrow window, the sidebar and side panel give way
  first.

## The heron

edualc's mascot is a heron (`assets/heron.svg`). It stands at the top of the sidebar and greets a
new session. It flies off when the conversation begins, and flies in when you open a new session
(`ui/heron.slint`, with the wing beats in `assets/heron-wings-*.svg`). About once a minute it looks
over its shoulder, preens or flaps its wings, for a second or so (`assets/heron-looking-back.svg`,
`heron-preening-*.svg`, `heron-flapping-*.svg`). It's also the app's icon
(`assets/io.github.felintusun.edualc.svg`, on Gruvbox's light background).
