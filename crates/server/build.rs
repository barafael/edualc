//! Writes the Lucide icons the window uses (those `ui/*.slint` names as `@lucide/<name>.svg`,
//! and the page's own `EXTRA`) as an enum for the page, so its wasm carries only those.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::PathBuf;

/// Icons the page uses that the Slint window doesn't.
const EXTRA: &[&str] = &[
    "external-link",
    "copy",
    "x",
    "check",
    "chevron-down",
    "chevron-up",
    "image-plus",
];

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("run by cargo"));
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("run by cargo"));
    let ui = manifest.join("../../ui");
    println!("cargo:rerun-if-changed={}", ui.display());
    let mut names: BTreeSet<String> = EXTRA.iter().map(|name| (*name).to_owned()).collect();
    for entry in std::fs::read_dir(&ui).expect("the window's ui folder") {
        let path = entry.expect("an entry").path();
        if path
            .extension()
            .is_some_and(|extension| extension == "slint")
        {
            let text = std::fs::read_to_string(&path).expect("a .slint file");
            for piece in text.split("@lucide/").skip(1) {
                // A comment's `@lucide/<name>.svg` aside.
                if let Some(name) = piece.split(".svg").next()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                {
                    names.insert(name.to_owned());
                }
            }
        }
    }
    let mut variants = String::new();
    let mut svgs = String::new();
    for icon in lucide_static_svg::Icon::all() {
        let name = icon.to_string();
        if names.remove(&name) {
            let variant: String = name
                .split('-')
                .map(|part| {
                    let mut chars = part.chars();
                    chars.next().map_or_else(String::new, |first| {
                        first.to_ascii_uppercase().to_string() + chars.as_str()
                    })
                })
                .collect();
            let svg = std::str::from_utf8(icon.svg_bytes()).expect("an SVG is text");
            writeln!(variants, "    {variant},").expect("written to a string");
            writeln!(svgs, "            Self::{variant} => {svg:?},").expect("written to a string");
        }
    }
    assert!(names.is_empty(), "icons Lucide hasn't: {names:?}");
    let code = format!(
        "/// The Lucide icons the window uses (the native window's too, not all of them the page's).\n#[allow(dead_code)]\n#[derive(Debug, Clone, Copy, PartialEq, Eq)]\npub enum Lucide {{\n{variants}}}\n\nimpl Lucide {{\n    /// Its SVG.\n    pub fn svg(self) -> &'static str {{\n        match self {{\n{svgs}        }}\n    }}\n}}\n"
    );
    std::fs::write(out.join("icons.rs"), code).expect("write the icons");
}
