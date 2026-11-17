//! The browser demo's daemon: Claude Code, played by a script. It takes what the window sends,
//! as the daemon on Linux does, and answers as `claude -p` would in stream-json, from a
//! scenario a word in the prompt picks (see [`super::scenarios`]), with pauses so the window
//! shows the answer come. Nothing leaves the page.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_channel::{Receiver, Sender};
use serde::Serialize;
use serde_json::{Map, Value};
use web_time::Duration;

use super::demo;
use super::scenarios;
use crate::app::{Event, Mode, PermissionMode};
use crate::claude::{Launch, Output};
use crate::external::wire::{
    ApiMessage, ContentBlock, ContextReport, ControlRequest, ControlResponse, Delta,
    IncomingRequest, Input, Message, MessageId, MessageStart, ModelUsage, PermissionResult, Reply,
    RequestId, ResultKind, SessionId, SideAnswer, StreamEvent, Suggestion, System, ToolName,
    ToolUseId, TurnResult, Usage, UserContent, UserLine,
};
use crate::ipc::{Act, ActFailure, ActId, Answer, Generation, PasteFailure, Place, Read, ToDaemon};

/// What the demo's Claude says it runs as.
const MODEL: &str = "claude-opus-5-5";
/// The pid the demo's sessions run as, in the window's eyes.
const PID: u32 = 4242;

/// Answers the window until it closes.
pub(crate) async fn run(requests: Receiver<ToDaemon>, events: Sender<Event>) {
    let mut sessions: HashMap<Generation, Session> = HashMap::new();
    while let Ok(request) = requests.recv().await {
        match request {
            ToDaemon::Start { key, launch } => {
                let session = Session::start(key, events.clone(), &launch);
                sessions.insert(key, session);
            }
            ToDaemon::Input { key, text, last } => {
                if let Some(session) = sessions.get_mut(&key) {
                    session.pieces.push_str(&text);
                    if last {
                        let line = std::mem::take(&mut session.pieces);
                        session.take(&line);
                    }
                }
            }
            ToDaemon::End { key } => {
                if sessions.remove(&key).is_some() {
                    tell(&events, key, Output::Ended(Some(0)));
                }
            }
            ToDaemon::Act { request, act } => acted(&events, request, act),
            ToDaemon::Read { request, read } => read_for(&events, request, read),
            // Nothing is written to a disk, and nothing in the demo changes but by the script;
            // the page's daemon serves this window alone, and ends with the page.
            ToDaemon::Hello { .. }
            | ToDaemon::Attach { .. }
            | ToDaemon::Detach { .. }
            | ToDaemon::Identify { .. }
            | ToDaemon::ShutDown
            | ToDaemon::ChangeSettings(_)
            | ToDaemon::Watch { .. }
            | ToDaemon::Unwatch(_) => {}
        }
    }
}

/// What a window reads, from the demo project.
fn read_for(events: &Sender<Event>, request: ActId, read: Read) {
    let mut lines = Vec::new();
    let answer = match read {
        Read::Live => Answer::Live(demo::running()),
        Read::Projects => Answer::Projects(demo::projects()),
        Read::Overview => Answer::Overview(demo::overview()),
        Read::Sessions { cwd, limit, .. } => {
            let mut sessions = demo::saved(&cwd);
            sessions.truncate(limit);
            Answer::Sessions(sessions)
        }
        Read::Transcript { place, from } => {
            let path = match place {
                Place::Path(path) => path,
                Place::Session { cwd, id } => cwd.join(format!("{}.jsonl", id.as_str())),
                // Beside its session's, as Claude Code keeps it: not its session's own.
                Place::Subagent { cwd, id, task, .. } => crate::sessions::transcript_of(
                    &cwd.join(format!("{}.jsonl", id.as_str())),
                    &task,
                ),
            };
            let read = match demo::transcript(&path) {
                Some(text) => {
                    let rest = usize::try_from(from)
                        .ok()
                        .and_then(|at| text.get(at..))
                        .unwrap_or_default();
                    lines = rest.lines().map(str::to_owned).collect();
                    Ok(u64::try_from(text.len()).unwrap_or(u64::MAX))
                }
                None => Err(crate::ipc::Unread::NotWritten),
            };
            Answer::Transcript { path, read }
        }
        Read::WorktreeOf { .. } | Read::LinkedWorktree { .. } => Answer::Worktree(None),
        Read::TypedFolder(typed) => Answer::Folder(demo_folder(Path::new(typed.trim()))),
        Read::Folder(path) => Answer::Folder(demo_folder(&path)),
        // The demo's sessions change no folder.
        Read::Changes { .. } => Answer::Changes(Ok(crate::changes::ChangeSet::default())),
        Read::Diff { .. } => Answer::Diff(Ok(crate::changes::Diff::default())),
    };
    let event = Event::Answered {
        request,
        answer,
        lines: crate::sessions::parse_lines(&lines),
    };
    if events.try_send(event).is_err() {
        tracing::debug!("the window is gone");
    }
}

/// One of the demo's folders, or why a path isn't one.
fn demo_folder(path: &Path) -> Result<std::path::PathBuf, String> {
    demo::projects()
        .into_iter()
        .map(|project| project.path)
        .find(|project| project == path)
        .ok_or_else(|| format!("{} isn't one of the demo's folders", path.display()))
}

fn tell(events: &Sender<Event>, generation: Generation, output: Output) {
    if events
        .try_send(Event::Claude { generation, output })
        .is_err()
    {
        tracing::debug!("the window is gone");
    }
}

/// What the desktop is asked, answered as a browser can.
fn acted(events: &Sender<Event>, request: ActId, act: Act) {
    let event = match act {
        Act::CheckTrust { folder } => Event::Trusted {
            request,
            trust: match demo::untrusted(&folder) {
                Some(runs) => crate::ipc::FolderTrust::Untrusted(runs),
                None => crate::ipc::FolderTrust::Accepted,
            },
        },
        Act::PickFolder => Event::Picked {
            request,
            folder: None,
        },
        Act::PasteImage => Event::Pasted {
            request,
            pasted: Err(PasteFailure::NoImage),
        },
        // A page has no keyring to keep a key in.
        Act::CheckApiKey => Event::ApiKeyStored(Ok(false)),
        Act::StoreApiKey(_) | Act::RemoveApiKey => Event::Acted {
            request,
            result: Err(ActFailure::Keyring(
                "the browser demo keeps no API key".to_owned(),
            )),
        },
        Act::CreateProject { .. } => Event::ProjectCreated {
            request,
            result: Err(crate::ipc::CreateProjectError::InBrowser),
        },
        // Not answered.
        Act::ChangesLooked { .. } | Act::MarkReviewed { .. } | Act::UnmarkReviewed { .. } => {
            return;
        }
        Act::OpenIn { .. }
        | Act::OpenUrl(_)
        | Act::Notify { .. }
        | Act::FocusWindowOf { .. }
        | Act::StopSession(_) => Event::Acted {
            request,
            result: Ok(()),
        },
    };
    if events.try_send(event).is_err() {
        tracing::debug!("the window is gone");
    }
}

/// Where the demo's Claude writes: the window, as one session's output.
#[derive(Debug, Clone)]
pub(super) struct Out {
    key: Generation,
    events: Sender<Event>,
    session: SessionId,
    cwd: PathBuf,
}

impl Out {
    /// One stream-json line, as `claude -p` writes it.
    pub(super) fn line(&self, message: &Message) {
        match serde_json::to_string(message) {
            Ok(line) => tell(&self.events, self.key, Output::of_line(line)),
            Err(error) => tracing::warn!(%error, "a line the demo can't write"),
        }
    }

    /// Answers request `request_id` with `response`, or with why it can't be written.
    fn respond(&self, request_id: RequestId, response: &impl Serialize) {
        let response = match serde_json::to_value(response) {
            Ok(response) => ControlResponse::Success {
                request_id,
                response,
            },
            Err(error) => ControlResponse::Error {
                request_id,
                error: error.to_string(),
            },
        };
        self.line(&Message::ControlResponse { response });
    }
}

/// What the window says to a turn under way.
#[derive(Debug)]
pub(super) enum Said {
    /// The answer to the permission request or question it waits on.
    Answer(PermissionResult),
    Interrupt,
}

/// One session the demo's Claude runs.
#[derive(Debug)]
struct Session {
    out: Out,
    model: String,
    mode: PermissionMode,
    turns: u32,
    /// The turn under way, to tell what the window says to it.
    turn: Option<Sender<Said>>,
    /// A line coming in pieces.
    pieces: String,
}

impl Session {
    fn start(key: Generation, events: Sender<Event>, launch: &Launch) -> Self {
        tell(&events, key, Output::Started { pid: PID, turns: 0 });
        Self {
            out: Out {
                key,
                events,
                session: launch.conversation.id().clone(),
                cwd: launch.cwd.clone(),
            },
            model: launch.model.clone().unwrap_or_else(|| MODEL.to_owned()),
            mode: PermissionMode::Default,
            turns: 0,
            turn: None,
            pieces: String::new(),
        }
    }

    /// A line the window wrote to the process.
    fn take(&mut self, line: &str) {
        let input = match serde_json::from_str::<Input>(line) {
            Ok(input) => input,
            Err(error) => {
                tracing::warn!(%error, "a line from the window the demo can't read");
                return;
            }
        };
        match input {
            Input::ControlRequest {
                request_id,
                request,
            } => self.control(request_id, request),
            Input::ControlResponse {
                response: Reply::Success { response, .. },
            } => self.tell_turn(Said::Answer(response)),
            Input::ControlResponse {
                response: Reply::Error { error, .. },
            } => tracing::warn!(%error, "the window turned a request down"),
            Input::User { message, .. } => self.prompt(&message.content),
            Input::BashCommand { command, .. } => scenarios::shell(&self.out, &command),
        }
    }

    fn tell_turn(&self, said: Said) {
        if let Some(turn) = &self.turn
            && turn.try_send(said).is_err()
        {
            tracing::debug!("no turn is under way");
        }
    }

    fn control(&mut self, request_id: RequestId, request: ControlRequest) {
        let nothing = Map::new();
        match request {
            ControlRequest::Initialize { .. } => {
                self.out.respond(request_id, &scenarios::initialized());
            }
            ControlRequest::Interrupt => {
                self.tell_turn(Said::Interrupt);
                self.out.respond(request_id, &nothing);
            }
            ControlRequest::SetModel { model } => {
                self.model = Some(model)
                    .filter(|model| !model.is_empty())
                    .unwrap_or_else(|| MODEL.to_owned());
                self.out.respond(request_id, &nothing);
            }
            ControlRequest::SetPermissionMode { mode } => {
                self.mode = mode;
                self.out.respond(request_id, &nothing);
            }
            ControlRequest::SideQuestion { question, .. } => {
                // Answered after a moment, as a model would; the window waits for it.
                let out = self.out.clone();
                let answered = slint::spawn_local(async move {
                    pause(Duration::from_millis(900)).await;
                    let answer = SideAnswer {
                        response: scenarios::side_answer(&question).to_owned(),
                        synthetic: false,
                    };
                    out.respond(request_id, &answer);
                });
                if let Err(error) = answered {
                    tracing::warn!(%error, "no side answer");
                }
            }
            ControlRequest::GetContextUsage => {
                let report = ContextReport {
                    categories: Vec::new(),
                    total_tokens: 38_200,
                    max_tokens: 200_000,
                };
                self.out.respond(request_id, &report);
            }
            ControlRequest::StopTask { .. }
            | ControlRequest::GetTaskOutput { .. }
            | ControlRequest::SetProgramMode { .. }
            | ControlRequest::GetUsage => self.out.respond(request_id, &nothing),
        }
    }

    /// A prompt: a turn plays the scenario it picks.
    fn prompt(&mut self, content: &crate::external::wire::Content) {
        use crate::external::wire::{Block, Content};
        let prompt = match content {
            Content::Text(text) => text.clone(),
            Content::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    Block::Text { text } => Some(text.as_str()),
                    Block::Image(_) => None,
                })
                .collect::<Vec<_>>()
                .join(" "),
        };
        self.turns += 1;
        let (said, inbox) = async_channel::unbounded();
        self.turn = Some(said);
        let turn = Turn {
            out: self.out.clone(),
            inbox,
            model: self.model.clone(),
            number: self.turns,
        };
        self.out.line(&Message::System(System::Init {
            session_id: self.out.session.clone(),
            model: self.model.clone(),
            permission_mode: Mode::Known(self.mode),
            cwd: self.out.cwd.clone(),
        }));
        if let Err(error) = slint::spawn_local(scenarios::play(turn, prompt)) {
            tracing::warn!(%error, "the turn couldn't start");
        }
    }
}

/// A tool's input, from its type in [`crate::external::tool_input`] or the wire's. Fails only
/// for a path that isn't UTF-8, which the demo's aren't: logged, the call shows no input.
pub(super) fn input(typed: &impl Serialize) -> Value {
    serde_json::to_value(typed).unwrap_or_else(|error| {
        tracing::warn!(%error, "a tool input the demo can't write");
        Value::Null
    })
}

/// The turn under way: where it writes, and what the window says to it.
#[derive(Debug)]
pub(super) struct Turn {
    pub(super) out: Out,
    inbox: Receiver<Said>,
    model: String,
    number: u32,
}

/// The window interrupted the turn.
#[derive(Debug)]
pub(super) struct Interrupted;

impl Turn {
    /// Stops at an interrupt the window sent meanwhile.
    fn heard(&self) -> Result<(), Interrupted> {
        match self.inbox.try_recv() {
            Ok(Said::Interrupt) => Err(Interrupted),
            Ok(Said::Answer(_)) | Err(_) => Ok(()),
        }
    }

    pub(super) async fn wait(&self, milliseconds: u64) -> Result<(), Interrupted> {
        pause(Duration::from_millis(milliseconds)).await;
        self.heard()
    }

    fn assistant(&self, content: Vec<ContentBlock>, id: MessageId) {
        self.out.line(&Message::Assistant {
            message: ApiMessage {
                id,
                content,
                usage: Usage {
                    input_tokens: 2400,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 18_000,
                    output_tokens: 350,
                },
            },
            parent_tool_use_id: None,
            error: None,
        });
    }

    /// Text, streamed as it is written, then whole.
    pub(super) async fn say(&self, text: &str) -> Result<(), Interrupted> {
        let id = MessageId::from(format!("msg_{}", uuid::Uuid::new_v4().simple()));
        let event = |event: StreamEvent| {
            self.out.line(&Message::StreamEvent {
                event,
                parent_tool_use_id: None,
            });
        };
        event(StreamEvent::MessageStart {
            message: MessageStart { id: id.clone() },
        });
        event(StreamEvent::ContentBlockStart {
            index: 0,
            content_block: ContentBlock::Text {
                text: String::new(),
            },
        });
        let characters: Vec<char> = text.chars().collect();
        for piece in characters.chunks(24) {
            event(StreamEvent::ContentBlockDelta {
                index: 0,
                delta: Delta::Text {
                    text: piece.iter().collect(),
                },
            });
            self.wait(20).await?;
        }
        event(StreamEvent::ContentBlockStop { index: 0 });
        self.assistant(
            vec![ContentBlock::Text {
                text: text.to_owned(),
            }],
            id,
        );
        Ok(())
    }

    /// A tool call; its id, for its result.
    pub(super) fn call(&self, name: ToolName, input: Value) -> ToolUseId {
        let id = ToolUseId::from(format!("toolu_{}", uuid::Uuid::new_v4().simple()));
        self.assistant(
            vec![ContentBlock::ToolUse {
                id: id.clone(),
                name,
                input,
            }],
            MessageId::from(format!("msg_{}", uuid::Uuid::new_v4().simple())),
        );
        id
    }

    pub(super) fn result(&self, id: &ToolUseId, content: UserContent, is_error: bool) {
        self.out
            .line(&Message::User(UserLine::of_session(UserContent::Blocks(
                vec![ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: Some(content),
                    is_error,
                }],
            ))));
    }

    /// A tool call that needs no asking, its result after `milliseconds`.
    pub(super) async fn run(
        &self,
        name: ToolName,
        input: Value,
        output: &str,
        milliseconds: u64,
    ) -> Result<(), Interrupted> {
        let id = self.call(name, input);
        self.wait(milliseconds).await?;
        self.result(&id, UserContent::Text(output.to_owned()), false);
        Ok(())
    }

    /// Asks the window to allow tool call `id`; its answer, as the window gave it.
    pub(super) async fn ask(
        &self,
        name: ToolName,
        input: Value,
        id: &ToolUseId,
        suggestions: &[Suggestion],
    ) -> Result<PermissionResult, Interrupted> {
        self.out.line(&Message::ControlRequest {
            request_id: RequestId::from(format!("perm_{id}")),
            request: IncomingRequest::CanUseTool {
                tool_name: name,
                input,
                permission_suggestions: suggestions.iter().map(self::input).collect(),
                description: String::new(),
                tool_use_id: Some(id.clone()),
            },
        });
        match self.inbox.recv().await {
            Ok(Said::Answer(answer)) => Ok(answer),
            Ok(Said::Interrupt) | Err(_) => Err(Interrupted),
        }
    }

    /// The turn's end.
    pub(super) fn finish(&self, text: &str) {
        self.out.line(&Message::Result(TurnResult {
            subtype: ResultKind::Success,
            is_error: false,
            result: Some(text.to_owned()),
            errors: Vec::new(),
            total_cost_usd: Some(0.0123 * f64::from(self.number)),
            model_usage: HashMap::from([(
                self.model.clone(),
                ModelUsage {
                    context_window: Some(200_000),
                },
            )]),
            api_error_status: None,
            duration_ms: Some(4200),
            usage: Usage {
                input_tokens: 2400,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 350,
            },
        }));
    }

    /// The turn's end, as an interrupt makes it.
    pub(super) fn interrupted(&self) {
        self.out
            .line(&Message::User(UserLine::of_session(UserContent::Blocks(
                vec![ContentBlock::Text {
                    text: "[Request interrupted by user]".to_owned(),
                }],
            ))));
        self.finish("interrupted");
    }

    /// The folder the turn works in, for the paths its tools name.
    pub(super) fn path(&self, relative: &str) -> PathBuf {
        self.out.cwd.join(relative)
    }
}

/// Waits `duration` on the page's clock.
pub(super) async fn pause(duration: Duration) {
    let (done, waited) = async_channel::bounded::<()>(1);
    slint::Timer::single_shot(duration, move || {
        if done.try_send(()).is_err() {
            tracing::debug!("nobody waits any more");
        }
    });
    if waited.recv().await.is_err() {
        tracing::debug!("the timer went");
    }
}
