//! The manual page, `assets/edualc.1`: the sections clap_mangen writes from the command line's
//! definition, then exit status, environment, files and see also.

use clap::CommandFactory as _;
use clap_mangen::roff::{Roff, bold, roman};

use super::Args;

const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/edualc.1");

/// An indented paragraph: `term` in bold, then `text`.
fn entry(roff: &mut Roff, term: &str, text: &str) {
    roff.control("TP", []);
    roff.text([bold(term)]);
    roff.text([roman(text)]);
}

fn manual() -> String {
    let man = clap_mangen::Man::new(Args::command())
        .source("edualc")
        .manual("edualc");
    let mut page = Vec::new();
    for render in [
        clap_mangen::Man::render_title,
        clap_mangen::Man::render_name_section,
        clap_mangen::Man::render_synopsis_section,
        clap_mangen::Man::render_description_section,
        clap_mangen::Man::render_options_section,
    ] {
        render(&man, &mut page).expect("writing to memory");
    }
    let mut roff = Roff::new();

    roff.control("SH", ["EXIT STATUS"]);
    entry(&mut roff, "0", "The window was closed.");
    entry(
        &mut roff,
        "1",
        "edualc couldn't start: its daemon, its confinement or its window.",
    );
    entry(&mut roff, "2", "The command line was wrong.");

    roff.control("SH", ["ENVIRONMENT"]);
    entry(
        &mut roff,
        "CLAUDE_BIN",
        "The Claude Code program to run; claude on the PATH if unset.",
    );
    entry(
        &mut roff,
        "CLAUDE_CONFIG_DIR",
        "Claude Code's configuration folder; ~/.claude if unset.",
    );
    entry(
        &mut roff,
        "RUST_LOG",
        "Which messages are logged to standard error, as a tracing-subscriber filter; edualc=info if unset.",
    );
    entry(
        &mut roff,
        "LC_ALL, LC_MESSAGES, LANG",
        "The first set to a Chinese, Japanese or Korean locale decides how text in Han characters alone is set, until the settings choose.",
    );
    entry(
        &mut roff,
        "NIRI_SOCKET",
        "Under niri, the sidebar can focus the window of a session running elsewhere.",
    );
    entry(
        &mut roff,
        "XDG_CONFIG_HOME, XDG_STATE_HOME, XDG_RUNTIME_DIR",
        "Where the files below are kept.",
    );

    roff.control("SH", ["FILES"]);
    entry(
        &mut roff,
        "$XDG_CONFIG_HOME/edualc/settings.json",
        "The settings (~/.config if unset).",
    );
    entry(
        &mut roff,
        "$XDG_STATE_HOME/edualc/claude/",
        "Claude Code's home for each project's sandboxed sessions (~/.local/state if unset).",
    );
    entry(
        &mut roff,
        "$XDG_RUNTIME_DIR/edualc/",
        "The daemon's socket.",
    );
    entry(
        &mut roff,
        "$CLAUDE_CONFIG_DIR/projects/",
        "Claude Code's transcripts, read for the sidebar and to continue sessions.",
    );

    roff.control("SH", ["SEE ALSO"]);
    roff.text([roman("Claude Code: https://code.claude.com/docs/")]);
    roff.control("br", []);
    roff.text([roman("edualc: https://github.com/felintusun/edualc")]);

    roff.to_writer(&mut page).expect("writing to memory");
    String::from_utf8(page).expect("roff is UTF-8")
}

#[test]
fn the_manual_matches_the_command_line() {
    let kept = std::fs::read_to_string(PATH).unwrap_or_default();
    assert!(
        kept == manual(),
        "assets/edualc.1 is out of date: cargo test -- --ignored write_the_manual"
    );
}

#[test]
#[ignore = "writes assets/edualc.1"]
fn write_the_manual() {
    std::fs::write(PATH, manual()).expect("assets/ is writable");
}
