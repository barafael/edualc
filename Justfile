# edualc's tasks: `just` lists them, `just --usage <recipe>` shows one's arguments. Recipes run in
# Nushell. Packaging's own commands: `nu dist/build.nu --help`; the CI images':
# `nu ci/images.nu --help`.

set shell := ["nu", "-c"]

default:
    @just --list

# Everything the CI workflow checks; `just in-image arch check` runs it in CI's image.
[group('check')]
check: fmt-check lint lint-wasm test present-check deny audit workflows packaging-check images-check licences-check

# One package's quick loop, for working on it: formats, then ast-grep's rules, clippy and the
# package's tests (or those whose names contain `filter`), each in one line per finding,
# stopping at the first stage that fails. A change to edualc-core needs edualc-host's and
# edualc's loops too, as they build on it; `just lint` and `just test` before handing in.
[group('check')]
[arg('package', pattern='edualc-core|edualc-host|edualc', help='the package')]
feedback package filter="":
    cargo fmt
    ast-grep scan --report-style short
    cargo clippy --locked --quiet -p {{package}} --all-targets --message-format short -- -D warnings
    python3 -c 'import os, sys; os.closerange(3, os.sysconf("SC_OPEN_MAX")); os.execvp(sys.argv[1], sys.argv[1:])' cargo test --locked --quiet -p {{package}} {{filter}}

# ast-grep's rules alone, one line per finding: no build, so it answers at once.
[group('check')]
scan:
    ast-grep scan --report-style short

# The presentation model is what scripts/present_gen.py writes from the Slint types.
[group('check')]
present-check: present
    git diff --exit-code crates/core/src/present/model.rs crates/slint/src/ui/model.rs

# The Rust sources' formatting (the Nickel files' is in packaging-check).
[group('check')]
fmt-check:
    cargo fmt --check

# ast-grep's rules and their tests, clippy, and the docs' links.
[group('check')]
lint: && doc
    ast-grep test
    ast-grep scan
    cargo clippy --locked --all-targets -- -D warnings

# The docs, private items included; a link that doesn't resolve fails (see Cargo.toml's lints).
[group('check')]
doc:
    cargo doc --locked --workspace --no-deps --document-private-items

# Clippy on the browser demo's build, where what only the daemon calls is unused.
[group('check')]
lint-wasm:
    cargo clippy --locked -p edualc-core -p edualc --target wasm32-unknown-unknown -- -D warnings -A dead_code

# The image decoder's confined helper refuses to start with file descriptors open past the
# standard streams, so the tests run with the others closed.
[doc('The tests.')]
[group('check')]
test:
    python3 -c 'import os, sys; os.closerange(3, os.sysconf("SC_OPEN_MAX")); os.execvp(sys.argv[1], sys.argv[1:])' cargo test --locked --quiet

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
regenerate: licences manual kept present

# The presentation model, from ui/types.slint and ui/state.slint: after changing either.
[group('write')]
present:
    python3 scripts/present_gen.py
    cargo fmt

# THIRD_PARTY_LICENSES.md, then the grammars' licences, which cargo-about doesn't see.
[group('write')]
licences:
    cargo about generate about.hbs | save --force THIRD_PARTY_LICENSES.md
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
