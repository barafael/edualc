# edualc's tasks; `just` lists them. Recipes run in Nushell. Packaging is in dist/build.nu.

set shell := ["nu", "-c"]

default:
    @just --list

# Formats the Rust and Nickel sources.
fmt:
    cargo fmt
    nickel format ...(glob dist/*.ncl)

# Everything CI checks.
check: fmt-check lint lint-wasm test deny packaging-check

fmt-check:
    cargo fmt --check

# ast-grep's rules and their tests, and clippy.
lint:
    ast-grep test
    ast-grep scan
    cargo clippy --locked --all-targets -- -D warnings

# Clippy on the browser demo's build.
lint-wasm:
    cargo clippy --locked --target wasm32-unknown-unknown -- -D warnings -A dead_code

# The image decoder's confined helper refuses to start with file descriptors open past the
# standard streams, so the tests run with the others closed.
[doc('The tests.')]
test:
    python3 -c 'import os, sys; os.closerange(3, os.sysconf("SC_OPEN_MAX")); os.execvp(sys.argv[1], sys.argv[1:])' cargo test --locked

# Licences, bans and sources of the dependencies.
deny:
    cargo deny check

# The packaging files: Nickel formatting and contracts, kept files, linters.
packaging-check:
    nu dist/build.nu check

# Writes every generated file kept in the tree.
regenerate: licences manual kept

# THIRD_PARTY_LICENSES.md, then the grammars' licences, which cargo-about doesn't see.
licences:
    cargo about generate about.hbs | save --force THIRD_PARTY_LICENSES.md
    cargo test --locked add_the_grammars_licenses_to_the_report -- --ignored

# assets/edualc.1, from the command line's definition.
manual:
    cargo test --locked write_the_manual -- --ignored

# The -git PKGBUILD and the AppStream metainfo.
kept:
    nu dist/build.nu write-kept

# Run in the tagged tree; the diff is for review, nothing is committed.
[doc('After tagging: the tarball checksum and the release PKGBUILD.')]
release-checksum:
    nu dist/build.nu release-checksum
    git diff -- dist/release.ncl dist/aur/edualc/PKGBUILD

# A package of the release build: deb, rpm or tarball.
package format:
    nu dist/build.nu {{format}}

# wasm-bindgen's CLI must match the crate's version in Cargo.lock.
[doc('The browser demo, into `site`.')]
demo site="target/site":
    #!/usr/bin/env nu
    let locked = cargo metadata --locked --format-version 1 | from json | get packages | where name == "wasm-bindgen" | first | get version
    let installed = wasm-bindgen --version | split row " " | last
    if $locked != $installed {
        error make { msg: $"wasm-bindgen ($installed) installed, ($locked) locked: cargo install --locked wasm-bindgen-cli --version ($locked)" }
    }
    cargo build --locked --profile web --target wasm32-unknown-unknown
    wasm-bindgen --target web --no-typescript --remove-name-section --out-dir {{site}} --out-name edualc target/wasm32-unknown-unknown/web/edualc.wasm
    cp web/index.html ...(glob web/fonts/*-OFL.txt) {{site}}
    cp assets/io.github.felintusun.edualc.svg $"{{site}}/edualc.svg"
