# edualc's tasks: `just` lists them, `just --usage <recipe>` shows one's arguments. Recipes run in
# Nushell. Packaging's own commands: `nu dist/build.nu --help`; the CI images':
# `nu ci/images.nu --help`.

set shell := ["nu", "-c"]

default:
    @just --list

# Everything the CI workflow checks; `just in-image arch check` runs it in CI's image.
[group('check')]
check: fmt-check lint lint-wasm test deny audit workflows packaging-check images-check licences-check

# The Rust sources' formatting (the Nickel files' is in packaging-check).
[group('check')]
fmt-check:
    cargo fmt --check

# ast-grep's rules and their tests, and clippy.
[group('check')]
lint:
    ast-grep test
    ast-grep scan
    cargo clippy --locked --all-targets -- -D warnings
    # edualcd: the daemon and the page server (the page itself is checked in `lint-wasm`)
    cargo clippy --locked -p edualc-server --features server --all-targets -- -D warnings

# Clippy on the browser demo's build, where what only the daemon calls is unused.
[group('check')]
lint-wasm:
    cargo clippy --locked -p edualc-core -p edualc --target wasm32-unknown-unknown -- -D warnings -A dead_code
    cargo clippy --locked -p edualc-server --features web --target wasm32-unknown-unknown -- -D warnings -A dead_code

# The image decoder's confined helper refuses to start with file descriptors open past the
# standard streams, so the tests run with the others closed.
[doc('The tests.')]
[group('check')]
test:
    python3 -c 'import os, sys; os.closerange(3, os.sysconf("SC_OPEN_MAX")); os.execvp(sys.argv[1], sys.argv[1:])' cargo test --locked
    cargo test --locked -p edualc-server --features server

# Licences, bans and sources of the dependencies.
[group('check')]
deny:
    cargo deny check

# RustSec's advisories against Cargo.lock.
[group('check')]
audit:
    cargo audit

# zizmor on the workflows; online audits too where GH_TOKEN is set.
[group('check')]
workflows *options:
    zizmor --persona auditor {{options}} .github

# The packaging files: Nickel formatting, types and contracts, kept files, release tags, linters.
[group('check')]
packaging-check:
    nu dist/build.nu check

# The workflows' image pins agree, and ci/Containerfile's Rust is rust-toolchain.toml's.
[group('check')]
images-check:
    nu ci/images.nu check

# THIRD_PARTY_LICENSES.md is what cargo-about writes for this tree.
[group('check')]
licences-check: licences
    git diff --exit-code THIRD_PARTY_LICENSES.md

# Formats the Rust and Nickel sources.
[group('write')]
fmt:
    cargo fmt
    nickel format ...(glob dist/*.ncl)

# Writes every generated file kept in the tree.
[group('write')]
regenerate: licences manual kept

# THIRD_PARTY_LICENSES.md, then the grammars' licences, which cargo-about doesn't see.
[group('write')]
licences:
    cargo about generate --workspace --features edualc-server/server about.hbs | save --force THIRD_PARTY_LICENSES.md
    cargo test --locked add_the_grammars_licenses_to_the_report -- --ignored

# assets/edualc.1, from the command line's definition.
[group('write')]
manual:
    cargo test --locked write_the_manual -- --ignored

# The desktop entry, the AppStream metainfo and the -git PKGBUILD, from dist/package.ncl.
[group('write')]
kept:
    nu dist/build.nu write-kept

# The diff is for review; nothing is committed.
[doc("After tagging and pushing the tag: its tarball's checksum and the release's PKGBUILD.")]
[group('release')]
release-checksum:
    nu dist/build.nu release-checksum
    git diff -- dist/releases.json dist/aur/edualc/PKGBUILD

# A package of target/release/edualc.
[group('package')]
[arg('format', pattern='deb|rpm|tarball', help='which package')]
package format:
    nu dist/build.nu {{format}}

# The libraries target/release/edualc links are those dist/package.ncl lists.
[group('package')]
check-linked:
    nu dist/build.nu check-linked

# Runs a recipe in a CI image (debian, fedora or arch) as the workflows pin it, with podman.
[group('ci')]
[arg('image', pattern='debian|fedora|arch', help='which CI image')]
in-image image +recipe:
    nu ci/images.nu run {{image}} {{recipe}}

# The programs a package holds: the window, and edualcd with the page it serves (dx bundle's
# `public/` folder), in target/release. The page is built with the `web` profile, which isn't
# stripped (wasm-bindgen refuses a stripped wasm) and is made small. Needs dioxus-cli (`cargo
# install --locked dioxus-cli`).
[doc('The window, and edualcd with its page, in target/release.')]
[group('package')]
release-build:
    #!/usr/bin/env nu
    cargo build --release --locked
    dx bundle --locked -p edualc-server --platform web --fullstack --profile web
    cp target/dx/edualcd/release/web/server target/release/edualcd
    rm -rf target/release/public
    cp -r target/dx/edualcd/release/web/public target/release/public

# wasm-bindgen's CLI must match the crate's version in Cargo.lock.
[doc('The browser demo, into `site`.')]
[group('demo')]
demo site="target/site":
    #!/usr/bin/env nu
    let locked = cargo metadata --locked --format-version 1 | from json | get packages | where name == "wasm-bindgen" | first | get version
    let installed = wasm-bindgen --version | split row " " | last
    if $locked != $installed {
        error make { msg: $"wasm-bindgen ($installed) installed, ($locked) locked: cargo install --locked wasm-bindgen-cli --version ($locked)" }
    }
    cargo build --locked -p edualc --profile web --target wasm32-unknown-unknown
    wasm-bindgen --target web --no-typescript --remove-name-section --out-dir {{site}} --out-name edualc target/wasm32-unknown-unknown/web/edualc.wasm
    cp web/index.html ...(glob web/fonts/*-OFL.txt) {{site}}
    cp assets/io.github.felintusun.edualc.svg $"{{site}}/edualc.svg"
