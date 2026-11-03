//! Compiles the UI and writes the Lucide icons it uses. The look is the app's own (Gruvbox,
//! `ui/theme.slint`); the Fluent style only lends its scroll bars and the desktop's light or
//! dark preference.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("run by cargo"));
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("run by cargo"));
    let icons = write_icons(&out);
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent".into())
        .with_library_paths(HashMap::from([("lucide".to_owned(), icons.clone())]));
    // The browser demo's window brings its fonts along (see `web/demo.slint`).
    let input = if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        manifest.join("web/demo.slint")
    } else {
        manifest.join("ui/app.slint")
    };
    let dependencies = slint_build::compile_with_output_path(&input, out.join("window.rs"), config)
        .unwrap_or_else(|error| panic!("failed to compile {}: {error}", input.display()));
    for path in dependencies {
        if !path.starts_with(&icons) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}

/// Writes every Lucide icon to `$OUT_DIR/lucide/<name>.svg`; Slint embeds only those used.
fn write_icons(out: &Path) -> PathBuf {
    let dir = out.join("lucide");
    std::fs::create_dir_all(&dir).expect("create the icon directory");
    for icon in lucide_static_svg::Icon::all() {
        std::fs::write(dir.join(format!("{icon}.svg")), icon.svg_bytes()).expect("write an icon");
    }
    dir
}
