#!/usr/bin/env nu

const root = path self ..
const repository = "ghcr.io/felintusun/edualc-ci"

# edualc's CI images (ci/Containerfile), as the workflows pin them by digest: which one is used,
# whether the pins and Rust's versions agree, and running a just recipe in one locally.
def main []: nothing -> nothing {
    help main
}

# Every pin of an image in the workflows: file, image, digest.
def pins []: nothing -> table<file: string, image: string, digest: string> {
    glob $"($root)/.github/workflows/*.yml"
    | each {|file|
        open --raw $file
        | parse --regex $"($repository):\(?<image>[a-z]+\)@\(?<digest>sha256:[0-9a-f]{64}\)"
        | insert file ($file | path relative-to $root)
    }
    | flatten
    | select file image digest
}

# The pinned reference of `image` (debian, fedora or arch).
def "main ref" [image: string]: nothing -> string {
    let digests = pins | where image == $image | get digest | uniq
    if ($digests | length) != 1 {
        error make { msg: $"($image) is pinned to ($digests | length) digests in the workflows" }
    }
    $"($repository):($image)@($digests | first)"
}

# Checks that each image has one digest across the workflows, and that ci/Containerfile's Rust
# is rust-toolchain.toml's.
def "main check" []: nothing -> nothing {
    for image in (pins | get image | uniq) {
        main ref $image | ignore
    }
    let channel = open $"($root)/rust-toolchain.toml" | get toolchain.channel
    let versions = (open --raw $"($root)/ci/Containerfile"
        | parse --regex '(?:rust:|rustup toolchain install |rustup default |RUSTUP_TOOLCHAIN=)(?<version>[0-9]+\.[0-9.]+[0-9])'
        | get version)
    let other = $versions | where $it != $channel | uniq
    if ($versions | is-empty) or ($other | is-not-empty) {
        error make { msg: $"ci/Containerfile has Rust ($versions | uniq | str join ', '), rust-toolchain.toml ($channel)" }
    }
}

# Runs `just <recipe>` in the pinned `image`, over a copy of this tree that's dropped afterwards.
def --wrapped "main run" [image: string, ...recipe: string]: nothing -> nothing {
    let reference = main ref $image
    (^podman run --rm -v $"($root):/src:O" -w /src $reference
        sh -c 'git config --system --add safe.directory /src && exec just "$@"' just ...$recipe)
}
