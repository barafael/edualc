//! Writes the Lucide icons the page's code names (`Lucide::<Icon>`) as an enum for the page, so its
//! wasm carries only those, and every one is used.

use std::fmt::Write as _;
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("run by cargo"));
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("run by cargo"));
    let sources = manifest.join("src");
    println!("cargo:rerun-if-changed={}", sources.display());
    let mut text = String::new();
    read_sources(&sources, &mut text);
    let mut variants = String::new();
    let mut svgs = String::new();
    for icon in lucide_static_svg::Icon::all() {
        let name = icon.to_string();
        let variant: String = name
            .split('-')
            .map(|part| {
                let mut chars = part.chars();
                chars.next().map_or_else(String::new, |first| {
                    first.to_ascii_uppercase().to_string() + chars.as_str()
                })
            })
            .collect();
        if names_icon(&text, &variant) {
            let svg = std::str::from_utf8(icon.svg_bytes()).expect("an SVG is text");
            writeln!(variants, "    {variant},").expect("written to a string");
            writeln!(svgs, "            Self::{variant} => {svg:?},").expect("written to a string");
        }
    }
    let code = format!(
        "/// The Lucide icons the page's code names (`Lucide::<Icon>`), each as its SVG.\n#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub enum Lucide {{\n{variants}}}\n\nimpl Lucide {{\n    /// Its SVG.\n    pub fn svg(self) -> &'static str {{\n        match self {{\n{svgs}        }}\n    }}\n}}\n"
    );
    std::fs::write(out.join("icons.rs"), code).expect("write the icons");
}

/// The text of every Rust source under `folder`.
fn read_sources(folder: &std::path::Path, into: &mut String) {
    for entry in std::fs::read_dir(folder).expect("the page's sources") {
        let path = entry.expect("an entry").path();
        if path.is_dir() {
            read_sources(&path, into);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            into.push_str(&std::fs::read_to_string(&path).expect("a source"));
        }
    }
}

/// Whether `text` says `Lucide::<variant>`, as a whole word.
fn names_icon(text: &str, variant: &str) -> bool {
    let needle = format!("Lucide::{variant}");
    text.match_indices(&needle).any(|(at, _)| {
        text[at + needle.len()..]
            .chars()
            .next()
            .is_none_or(|next| !next.is_alphanumeric() && next != '_')
    })
}
