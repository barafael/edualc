//! Writes the Lucide icons the window uses (those `ui/*.slint` names as `@lucide/<name>.svg`,
//! and the page's own `EXTRA`) as a lookup for the page, so its wasm carries only those.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// Icons the page uses that the Slint window doesn't.
const EXTRA: &[&str] = &[
    "external-link",
    "copy",
    "x",
    "check",
    "chevron-down",
    "chevron-up",
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
    let mut code = String::from(
        "/// The SVG of Lucide icon `name`, if the window uses it.\npub fn svg(name: &str) -> Option<&'static str> {\n    match name {\n",
    );
    for icon in lucide_static_svg::Icon::all() {
        let name = icon.to_string();
        if names.remove(&name) {
            let svg = std::str::from_utf8(icon.svg_bytes()).expect("an SVG is text");
            code.push_str(&format!("        {name:?} => Some({svg:?}),\n"));
        }
    }
    assert!(names.is_empty(), "icons Lucide hasn't: {names:?}");
    code.push_str("        _ => None,\n    }\n}\n");
    std::fs::write(out.join("icons.rs"), code).expect("write the icons");
}
