//! What the demo's Claude does, by a word in the prompt: fix trailhead's elevation gain (with
//! an edit to allow), run its benchmark (a command to allow), plan per-kilometre splits (a plan
//! to comment on), ask how to smooth (questions with previews), draw the gain chart (an image
//! in a tool's card); anything else is told what to try.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};

use super::daemon::{Interrupted, Out, Turn};
use super::demo;

/// Plays the turn for `prompt`, to its end or an interrupt.
pub(super) async fn play(turn: Turn, prompt: String) {
    let lowered = prompt.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|word| lowered.contains(word));
    let played = if has(&["smooth", "question"]) {
        questions(&turn).await
    } else if has(&["benchmark", "bench"]) {
        benchmark(&turn).await
    } else if has(&["split", "plan"]) {
        plan(&turn).await
    } else if has(&["chart", "draw", "plot", "image", "picture"]) {
        chart(&turn).await
    } else if has(&["elevation", "gain", "fix"]) {
        fix(&turn).await
    } else {
        guide(&turn).await
    };
    match played {
        Ok(said) => turn.finish(said),
        Err(Interrupted) => turn.interrupted(),
    }
}

const GUIDE: &str =
    "This is **edualc's demo**, running in your browser. The window is the real one, \
compiled to WebAssembly; Claude is played by a script, so nothing you type leaves the page.

Things to try:

- **Fix the elevation gain**: Claude reads, explains and edits, and asks before it changes a file
- **Run the benchmark**: a command waiting for your permission, with a rule to always allow it
- **Plan per-kilometre splits**: a plan you comment on part by part
- **How should we smooth elevation?**: questions with a preview beside each answer
- **Draw the gain chart**: an image in a tool's card
- `/btw why 3 m?`: a side question, answered beside the conversation
- `! git log --oneline`: a shell command

The sessions in the sidebar open too, the ones running elsewhere among them, and the palette \
button at the top switches the look.";

async fn guide(turn: &Turn) -> Result<&'static str, Interrupted> {
    turn.say(GUIDE).await?;
    Ok("Guided.")
}

const GAIN_OLD: &str = "pub fn summarize(points: &[Point]) -> Summary {
    let mut summary = Summary::default();
    for pair in points.windows(2) {
        let [from, to] = pair else { continue };
        summary.distance_km += from.distance_km(to);
        let climb_m = to.elevation_m - from.elevation_m;
        if climb_m > 0.0 {
            summary.gain_m += climb_m;
        } else {
            summary.loss_m -= climb_m;
        }
    }
    summary
}";

const GAIN_NEW: &str =
    "/// GPS elevation jitters by a few metres from fix to fix. A climb or descent counts once the
/// track has moved this far from the last point counted, so the jitter adds nothing.
const THRESHOLD_M: f64 = 3.0;

pub fn summarize(points: &[Point]) -> Summary {
    let mut summary = Summary::default();
    let Some(first) = points.first() else {
        return summary;
    };
    let mut anchor_m = first.elevation_m;
    for pair in points.windows(2) {
        let [from, to] = pair else { continue };
        summary.distance_km += from.distance_km(to);
        let climb_m = to.elevation_m - anchor_m;
        if climb_m >= THRESHOLD_M {
            summary.gain_m += climb_m;
            anchor_m = to.elevation_m;
        } else if climb_m <= -THRESHOLD_M {
            summary.loss_m -= climb_m;
            anchor_m = to.elevation_m;
        }
    }
    summary
}";

const GAIN_ANALYSIS: &str = "## What's going on

`summarize` adds up **every** rise between two consecutive points:

```rust
for pair in points.windows(2) {
    let [from, to] = pair else { continue };
    let climb_m = to.elevation_m - from.elevation_m;
    if climb_m > 0.0 {
        summary.gain_m += climb_m;
    }
}
```

GPS elevation jitters by 2 to 5 m from one fix to the next, so on a track with a point every second the noise alone climbs \
hundreds of metres:

| Track | Points | Gain now | Reference |
|---|---:|---:|---:|
| ridge-loop | 5,124 | 1,412 m | ~860 m |
| river-flat | 3,980 | 318 m | ~40 m |

The usual fix is a **hysteresis threshold**: a climb only counts once the track has moved at \
least a few metres away from the last point counted. 3 m is what most trackers use.";

const GAIN_DONE: &str = "Fixed. With the threshold the fixtures come out where they should:

| Track | Before | After | Reference |
|---|---:|---:|---:|
| ridge-loop | 1,412 m | **868 m** | ~860 m |
| river-flat | 318 m | **41 m** | ~40 m |

All 7 tests pass. Distance is unchanged, since it doesn't depend on elevation.";

async fn fix(turn: &Turn) -> Result<&'static str, Interrupted> {
    turn.say("I'll look at how the summary is computed first.")
        .await?;
    let stats = turn.path("src/stats.rs");
    turn.run(
        "Read",
        &json!({"file_path": stats}),
        "     1\t//! Distance and elevation for a track.\n     2\t\n     3\tuse crate::gpx::Point;",
        400,
    )
    .await?;
    turn.run(
        "Grep",
        &json!({"pattern": "elevation_m", "path": turn.path("src"), "output_mode": "content"}),
        "src/gpx.rs:6:    pub elevation_m: f64,\nsrc/stats.rs:17:        let climb_m = to.elevation_m - from.elevation_m;",
        350,
    )
    .await?;
    turn.run(
        "Bash",
        &json!({"command": "cargo run -q -- tests/fixtures/*.gpx",
                "description": "Summarize the fixture tracks"}),
        "ridge-loop     14.2 km  ↑  1412 m  ↓  1398 m\nriver-flat      9.8 km  ↑   318 m  ↓   321 m",
        800,
    )
    .await?;
    turn.say(GAIN_ANALYSIS).await?;
    let input = json!({"file_path": stats, "old_string": GAIN_OLD, "new_string": GAIN_NEW});
    let edit = turn.call("Edit", &input);
    let suggestions = json!([{"type": "setMode", "mode": "acceptEdits", "destination": "session"}]);
    let answer = turn.ask("Edit", &input, &edit, &suggestions).await?;
    if !allowed(&answer) {
        turn.result(&edit, &json!(declined(&answer)), true);
        turn.say("Left `stats.rs` as it was.").await?;
        return Ok("Declined.");
    }
    turn.result(
        &edit,
        &json!(format!("The file {stats} has been updated successfully.")),
        false,
    );
    turn.run(
        "Bash",
        &json!({"command": "cargo test", "description": "Run the tests"}),
        "running 7 tests\n.......\ntest result: ok. 7 passed; 0 failed; 0 ignored",
        1200,
    )
    .await?;
    turn.say(GAIN_DONE).await?;
    Ok("Fixed.")
}

async fn benchmark(turn: &Turn) -> Result<&'static str, Interrupted> {
    turn.say("I'll run the `summarize` benchmark, to see what the threshold costs.")
        .await?;
    let input = json!({"command": "cargo bench --bench summarize",
                       "description": "Run the summarize benchmark"});
    let bash = turn.call("Bash", &input);
    let suggestions = json!([{"type": "addRules", "behavior": "allow", "destination": "localSettings",
                              "rules": [{"toolName": "Bash", "ruleContent": "cargo bench:*"}]}]);
    let answer = turn.ask("Bash", &input, &bash, &suggestions).await?;
    if !allowed(&answer) {
        turn.result(&bash, &json!(declined(&answer)), true);
        turn.say("Skipped the benchmark.").await?;
        return Ok("Declined.");
    }
    turn.wait(1500).await?;
    turn.result(
        &bash,
        &json!(
            "summarize/ridge-loop   time:   [412.3 µs 415.0 µs 418.1 µs]\n                       change: [+1.2% +1.8% +2.5%]\n\
             summarize/river-flat   time:   [301.7 µs 303.2 µs 304.9 µs]\n                       change: [+0.9% +1.4% +2.0%]"
        ),
        false,
    );
    turn.say(
        "The threshold costs under 2%: about 415 µs for the 5,124 points of `ridge-loop`. \
         Reading the GPX takes far longer than that.",
    )
    .await?;
    Ok("Benchmarked.")
}

const SPLITS_PLAN: &str = "# Plan: per-kilometre splits

## Context

Runners and hikers want to see where a track got slow. `trailhead` prints one line per track; \
there is no way to see how the pace changed along the way.

## Changes

1. Add `stats::splits(points, every_km)`, which cuts the track wherever the distance passes a \
multiple of `every_km` and returns each part's distance, time, gain and loss.
2. Interpolate the point where a kilometre ends, so a split is exactly 1 km long and not wherever \
the next fix happens to be.
3. Add `--splits [KM]` to the command line, defaulting to 1 km, printed as a table below the summary.
4. Include the splits in `--json` as an array, with the field names of the summary.

## Not in this change

- Splits by time (every 10 minutes): later, once someone asks.
- Moving time versus elapsed time: tracks without timestamps would need a guess.

## Verification

- A test track of exactly 3.5 km gives three 1 km splits and one of 0.5 km.
- `cargo clippy --all-targets -- -D warnings` and `cargo test`.
";

async fn plan(turn: &Turn) -> Result<&'static str, Interrupted> {
    turn.say("I've read `stats.rs` and `main.rs`. Here's how I'd add splits.")
        .await?;
    let input = json!({"plan": SPLITS_PLAN});
    let exit = turn.call("ExitPlanMode", &input);
    let answer = turn.ask("ExitPlanMode", &input, &exit, &json!([])).await?;
    if allowed(&answer) {
        turn.result(
            &exit,
            &json!("User has approved your plan. You can now start coding."),
            false,
        );
        turn.say("Approved. I'll start with `stats::splits` and its test.")
            .await?;
        Ok("Approved.")
    } else {
        turn.result(&exit, &json!(declined(&answer)), true);
        turn.say(
            "Understood. I'll fold your comments into the plan: interpolating elevation and time \
             at each split's end, and the `splits` array keeping the summary's field order.",
        )
        .await?;
        Ok("Revising.")
    }
}

async fn questions(turn: &Turn) -> Result<&'static str, Interrupted> {
    turn.say("There are a few ways to do this. Two questions first.")
        .await?;
    let input = json!({"questions": [
        {"question": "How should noisy elevation be smoothed?", "header": "Smoothing", "multiSelect": false,
         "options": [
            {"label": "Threshold (3 m)",
             "description": "Count a climb once it passes 3 m. Simple, and what most trackers do.",
             "preview": "let rise = h - anchor;\nif rise.abs() >= 3.0 {\n    gain += rise.max(0.0);\n    anchor = h;\n}"},
            {"label": "Moving average",
             "description": "Average each point with its neighbours first. Smooths, but flattens real short climbs.",
             "preview": "let h: Vec<f64> = heights\n    .windows(5)\n    .map(mean)\n    .collect();"},
            {"label": "Digital elevation model",
             "description": "Replace GPS elevation with map data. Most accurate; needs a download per region."}]},
        {"question": "Which outputs should show the smoothed gain?", "header": "Outputs", "multiSelect": true,
         "options": [
            {"label": "Table", "description": "The default one-line summary."},
            {"label": "JSON", "description": "`--json`, for scripts."},
            {"label": "Splits", "description": "Each split's gain and loss."}]},
    ]});
    let ask = turn.call("AskUserQuestion", &input);
    let answer = turn
        .ask("AskUserQuestion", &input, &ask, &json!([]))
        .await?;
    let answers = answer
        .pointer("/updatedInput/answers")
        .and_then(Value::as_object)
        .map(|answers| {
            answers
                .iter()
                .map(|(question, picked)| format!("- **{question}** {}", picked_text(picked)))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    turn.result(
        &ask,
        &json!("User has answered your questions."),
        !allowed(&answer),
    );
    if answers.is_empty() {
        turn.say("No answers, so I'll keep the 3 m threshold for now.")
            .await?;
    } else {
        turn.say(&format!("Thanks. Going with what you chose:\n\n{answers}"))
            .await?;
    }
    Ok("Answered.")
}

fn picked_text(picked: &Value) -> String {
    picked
        .as_str()
        .map_or_else(|| picked.to_string(), str::to_owned)
}

async fn chart(turn: &Turn) -> Result<&'static str, Interrupted> {
    turn.run(
        "Bash",
        &json!({"command": "cargo run -q --example gain_chart -- tests/fixtures/ridge-loop.gpx docs/gain.png",
                "description": "Chart the cumulative gain of ridge-loop"}),
        "wrote docs/gain.png (960×400)",
        900,
    )
    .await?;
    let read = turn.call("Read", &json!({"file_path": turn.path("docs/gain.png")}));
    turn.wait(300).await?;
    turn.result(
        &read,
        &json!([{"type": "image", "source": {
            "type": "base64", "media_type": "image/png", "data": BASE64.encode(demo::GAIN_CHART)}}]),
        false,
    );
    turn.say(
        "The grey line counts every rise and pulls away from the start: by the end it is 550 m \
         over. The orange one, with the threshold, stays with Strava's ~860 m all the way. The \
         chart opens in the tool's card above.",
    )
    .await?;
    Ok("Charted.")
}

/// Whether the window allowed the call.
fn allowed(answer: &Value) -> bool {
    answer.get("behavior").and_then(Value::as_str) == Some("allow")
}

/// A denial's result, with what the user said to do instead.
fn declined(answer: &Value) -> String {
    let said = answer
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    format!("The user doesn't want to proceed with this tool use. {said}")
}

/// The answer to `initialize`: the models, commands and account the window lists.
pub(super) fn initialize() -> Value {
    json!({
        "models": [
            {"value": "default", "displayName": "Default (recommended)", "resolvedModel": "claude-opus-5-5",
             "description": "Opus 5.5 for most work"},
            {"value": "sonnet", "displayName": "Sonnet", "resolvedModel": "claude-sonnet-5-5",
             "description": "Sonnet 5.5, faster"},
            {"value": "haiku", "displayName": "Haiku", "resolvedModel": "claude-haiku-5-5",
             "description": "Haiku 5.5, fastest"}],
        "commands": [
            {"name": "compact", "description": "Clear the conversation but keep a summary",
             "argumentHint": "[instructions]"},
            {"name": "review", "description": "Review a pull request", "argumentHint": "[pr]"},
            {"name": "init", "description": "Write a CLAUDE.md for this project"},
            {"name": "cost", "description": "Show the cost of this session"}],
        "agents": [], "output_style": "default", "available_output_styles": ["default"],
        "account": {"email": "you@example.com", "subscriptionType": "max", "apiProvider": "firstParty"},
        "claude_code_version": "2.1.316",
    })
}

/// The answer to a side question (`/btw`).
pub(super) fn side_answer(question: &str) -> &'static str {
    let lowered = question.to_lowercase();
    if lowered.contains("3 m") || lowered.contains("threshold") {
        "3 m sits just above typical GPS jitter (2 to 5 m between fixes with a barometer, more \
         without). At 5 m, short real climbs on rolling terrain start to vanish: a 4 m rise and \
         fall repeated 50 times is 200 m you'd lose. Making it a `--threshold` option would let \
         people tune it."
    } else if lowered.contains("hysteresis") {
        "Hysteresis means the output only changes once the input has moved past a margin from \
         where it last changed, so small back-and-forth wiggles don't count. Here the margin is \
         3 m of elevation."
    } else {
        "On the side, without joining the conversation: the threshold only changes what counts \
         as a climb. Distance and time are untouched."
    }
}

/// A `!` command, run "in the project": a few are played, the rest say so.
pub(super) fn shell(out: &Out, command: &str) {
    let command = command.trim();
    let (stdout, stderr) = if command.starts_with("git log") {
        (
            "d4e1f2a Count climbs past a 3 m threshold\n9b0c7e1 Read routes as well as tracks\n\
             51aa3f0 Set up CI with cargo-deny and clippy",
            "",
        )
    } else if command == "ls" || command.starts_with("ls ") {
        (
            "Cargo.lock\nCargo.toml\nREADME.md\ndocs\nexamples\nsrc\ntests",
            "",
        )
    } else if command.starts_with("cargo test") {
        (
            "running 7 tests\n.......\ntest result: ok. 7 passed; 0 failed; 0 ignored",
            "",
        )
    } else {
        (
            "",
            "The demo plays only a few commands: git log, ls and cargo test.",
        )
    };
    let replayed = |content: String| {
        out.line(&json!({
            "type": "user", "session_id": out.session(), "isReplay": true,
            "message": {"role": "user", "content": content},
        }));
    };
    replayed(format!("<bash-input>{command}</bash-input>"));
    replayed(format!(
        "<bash-stdout>{stdout}</bash-stdout><bash-stderr>{stderr}</bash-stderr>"
    ));
}
