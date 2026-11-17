//! Writes the presentation model from the window's Slint types, for every frontend to share.
//!
//! From `ui/types.slint` (its enums and structs) and `ui/state.slint` (the presenter's globals'
//! `in` properties), read with Slint's own parser, it writes:
//!
//! - `crates/core/src/present/model.rs`: the same types as plain Rust data, which the app's
//!   presenter fills and every frontend shows;
//! - `crates/slint/src/ui/model.rs`: the Slint window's side, which turns them into Slint's types
//!   and brings a global in line with a new state.
//!
//! Both outputs are committed. `just present` writes them; `--check` fails when either differs
//! from what the Slint files say.

mod model;
mod write;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use crate::model::{Model, ReadError};

#[derive(Debug, Parser)]
#[command(about = "Writes edualc's presentation model from ui/types.slint and ui/state.slint")]
struct Args {
    /// Only check that the committed files are what the Slint files say; write nothing.
    #[arg(long)]
    check: bool,
}

/// Why the files couldn't be written or checked.
#[derive(Debug, thiserror::Error)]
enum GenError {
    #[error(transparent)]
    Read(#[from] ReadError),
    #[error("couldn't read {}", path.display())]
    ReadOutput {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("couldn't write {}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("couldn't run rustfmt on the text for {}", path.display())]
    Rustfmt {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("rustfmt refused the text for {} ({status}):\n{stderr}", path.display())]
    RustfmtFailed {
        path: PathBuf,
        status: std::process::ExitStatus,
        stderr: String,
    },
    #[error("out of date, run `just present`: {}", stale.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join(", "))]
    Stale { stale: Vec<PathBuf> },
}

/// `text` as `cargo fmt` leaves it, so the files written match the formatted ones committed.
fn formatted(root: &Path, path: &Path, text: &str) -> Result<String, GenError> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let failed = |source| GenError::Rustfmt {
        path: path.to_owned(),
        source,
    };
    let mut rustfmt = Command::new("rustfmt")
        .args(["--edition", "2024", "--emit", "stdout"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(failed)?;
    rustfmt
        .stdin
        .take()
        .ok_or_else(|| failed(std::io::Error::other("rustfmt's input wasn't piped")))?
        .write_all(text.as_bytes())
        .map_err(failed)?;
    let output = rustfmt.wait_with_output().map_err(failed)?;
    if !output.status.success() {
        return Err(GenError::RustfmtFailed {
            path: path.to_owned(),
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    String::from_utf8(output.stdout)
        .map_err(|error| failed(std::io::Error::new(std::io::ErrorKind::InvalidData, error)))
}

/// The repository's root, from this crate's place in it.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn run(args: &Args) -> Result<(), GenError> {
    let root = root();
    let model = Model::read(&root.join("ui/types.slint"), &root.join("ui/state.slint"))?;
    let outputs = [
        (
            root.join("crates/core/src/present/model.rs"),
            write::present(&model),
        ),
        (
            root.join("crates/slint/src/ui/model.rs"),
            write::slint(&model),
        ),
    ];
    let outputs = outputs
        .into_iter()
        .map(|(path, text)| formatted(&root, &path, &text).map(|text| (path, text)))
        .collect::<Result<Vec<_>, _>>()?;
    if args.check {
        let mut stale = Vec::new();
        for (path, text) in outputs {
            let committed =
                std::fs::read_to_string(&path).map_err(|source| GenError::ReadOutput {
                    path: path.clone(),
                    source,
                })?;
            if committed != text {
                stale.push(path);
            }
        }
        return if stale.is_empty() {
            Ok(())
        } else {
            Err(GenError::Stale { stale })
        };
    }
    for (path, text) in outputs {
        std::fs::write(&path, text).map_err(|source| GenError::Write { path, source })?;
    }
    Ok(())
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("present-gen: {error}");
            let mut source = std::error::Error::source(&error);
            while let Some(cause) = source {
                eprintln!("  because: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}
