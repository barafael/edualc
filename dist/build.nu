#!/usr/bin/env nu
# Packages edualc with each format's own tools, from dist/formats.ncl. Run from the repository's
# root, after `cargo build --release`:
#
#   nu dist/build.nu generate        packaging files from dist/*.ncl
#   nu dist/build.nu check           formatting, contracts, kept files, linters
#   nu dist/build.nu write-kept      writes the -git PKGBUILD and the metainfo into the tree
#   nu dist/build.nu write-release   writes the release's PKGBUILD
#   nu dist/build.nu release-checksum  the tag tarball's checksum into release.ncl, then write-release
#   nu dist/build.nu deb             .deb (dpkg-shlibdeps, dpkg-gencontrol, dpkg-deb)
#   nu dist/build.nu rpm             .rpm (rpmbuild)
#   nu dist/build.nu tarball         .tar.gz
#   nu dist/build.nu lint-deb DIR    lintian on the .deb files in DIR
#   nu dist/build.nu lint-rpm DIR    rpmlint on the .rpm files under DIR
#
# Each command writes to a new folder under target/packaging and prints its path on stdout; the
# tools' own output goes to stderr. Dates come from SOURCE_DATE_EPOCH, else the last commit.

const packaging = "target/packaging"

def main [] {
    help main
}

# A fresh folder for one run of `kind`.
def fresh [kind: string]: nothing -> path {
    mkdir $packaging
    mktemp --directory --tmpdir-path $packaging $"($kind).XXXXXX" | path expand
}

# When the source was last changed, in seconds since 1970.
def epoch []: nothing -> int {
    if ($env.SOURCE_DATE_EPOCH? | is-empty) {
        ^git log -1 --format=%ct | str trim | into int
    } else {
        $env.SOURCE_DATE_EPOCH | into int
    }
}

# The release date as Debian's changelog (RFC 2822) and RPM's write it. Day and month names are
# spelled out here because `format date` uses the locale's.
def release-dates []: nothing -> record<date_rfc2822: string, date_rpm: string> {
    const days = [Mon Tue Wed Thu Fri Sat Sun]
    const months = [Jan Feb Mar Apr May Jun Jul Aug Sep Oct Nov Dec]
    let when = (epoch) * 1_000_000_000 | into datetime
    let day = $days | get (($when | format date "%u" | into int) - 1)
    let month = $months | get (($when | format date "%m" | into int) - 1)
    let day_of_month = $when | format date "%d"
    let year = $when | format date "%Y"
    {
        date_rfc2822: $"($day), ($day_of_month) ($month) ($year) ($when | format date '%H:%M:%S') +0000",
        date_rpm: $"($day) ($month) ($day_of_month) ($year)",
    }
}

# Every release, newest first: the v* tags with their dates, and `version` if it has no tag yet,
# dated by the last commit.
def release-history [version: string]: nothing -> list<record<version: string, date: string>> {
    let tagged = (^git for-each-ref --sort=-creatordate
        --format "%(refname:short) %(creatordate:short)" "refs/tags/v*"
        | lines
        | parse "v{version} {date}")
    if ($tagged | any {|release| $release.version == $version }) {
        $tagged
    } else {
        let date = (epoch) * 1_000_000_000 | into datetime | format date "%Y-%m-%d"
        [{version: $version, date: $date}] ++ $tagged
    }
}

# A field of formats.ncl as text or JSON, with the `release` file merged in.
def field [name: string, release: path, --json (-j)]: nothing -> string {
    let format = if $json { "json" } else { "text" }
    ^nickel export --format $format dist/formats.ncl dist/release.ncl $release --field $name
}

# Installs `files` (source, path, mode, gzip) under `root`.
def stage [files: list<record<source: string, path: string, mode: string, gzip: bool>>, root: path] {
    for file in $files {
        let target = $root | path join ($file.path | str trim --left --char "/")
        if $file.gzip {
            mkdir ($target | path dirname)
            open --raw $file.source | ^gzip -9n | save --raw $target
            ^chmod $file.mode $target
        } else {
            ^install $"-Dm($file.mode)" $file.source $target
        }
    }
}

# Writes the packaging files from dist/*.ncl.
def "main generate" []: nothing -> path {
    let generated = fresh generated
    let version = open Cargo.toml | get package.version
    let release = $"($generated)/release.json"
    { release: ((release-dates) | merge {history: (release-history $version)}) } | to json | save $release
    ^nickel export --format text dist/package.ncl --field name --output $"($generated)/name"
    $version | save $"($generated)/version"
    mkdir $"($generated)/debian" $"($generated)/fedora" $"($generated)/tarball"
    mkdir $"($generated)/arch/edualc" $"($generated)/arch/edualc-git"
    for part in [control changelog copyright] {
        field $"debian.($part)" $release | save $"($generated)/debian/($part)"
    }
    field debian.files $release --json | save $"($generated)/debian/files.json"
    field fedora.spec $release | save $"($generated)/fedora/package.spec"
    field tarball.files $release --json | save $"($generated)/tarball/files.json"
    field arch.tagged $release | save $"($generated)/arch/edualc/PKGBUILD"
    field arch.git $release | save $"($generated)/arch/edualc-git/PKGBUILD"
    [
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<!-- Written from dist/package.ncl by dist/formats.ncl and dist/build.nu: change those, not this. -->'
        (field metainfo $release --json | from json | to xml --indent 2 --self-closed --partial-escape)
    ]
    | str join "\n"
    | $in + "\n"
    | save $"($generated)/metainfo.xml"
    $generated
}

# Generated files kept in the tree: path in the tree → path in `generate`'s folder. The release's
# PKGBUILD builds the last tag, so it is written by `write-release` instead.
const kept = {
    "dist/aur/edualc-git/PKGBUILD": "arch/edualc-git/PKGBUILD"
    "assets/io.github.felintusun.edualc.metainfo.xml": "metainfo.xml"
}

# Writes the kept files into the tree.
def "main write-kept" [] {
    let generated = main generate
    for file in ($kept | transpose kept written) {
        cp $"($generated)/($file.written)" $file.kept
    }
}

# Sets dist/release.ncl's checksum to that of the tag v<Cargo.toml's version>'s tarball as GitHub
# serves it, after comparing it with `git archive` of the tag, and writes the release's PKGBUILD.
def "main release-checksum" [] {
    let version = open Cargo.toml | get package.version
    let tag = $"v($version)"
    let url = $"(open Cargo.toml | get package.repository)/archive/refs/tags/($tag).tar.gz"
    let served = http get --raw $url | hash sha256
    let archived = (^git archive $"--prefix=(open Cargo.toml | get package.name)-($version)/" $tag
        | ^gzip -n
        | hash sha256)
    if $served != $archived {
        error make { msg: $"($url) is not `git archive` of ($tag): ($served) against ($archived)" }
    }
    open --raw dist/release.ncl
    | str replace --regex 'source_sha256 = "[0-9a-f]{64}"' $"source_sha256 = \"($served)\""
    | save --force dist/release.ncl
    main write-release
}

# Writes the release's PKGBUILD. Run in the tagged tree, after setting the tarball's checksum in
# dist/release.ncl.
def "main write-release" [] {
    let generated = main generate
    cp $"($generated)/arch/edualc/PKGBUILD" dist/aur/edualc/PKGBUILD
}

# Checks formatting and contracts of dist/*.ncl, compares the kept files with `generate`'s, and
# lints the PKGBUILDs, the desktop entry and the metainfo.
def "main check" [] {
    ^nickel format --check ...(glob dist/*.ncl)
    # Contracts are checked lazily, so export everything. `| ignore` would drop the exit code.
    ^nickel export dist/package.ncl --output /dev/null
    let generated = main generate
    for file in ($kept | transpose kept written) {
        ^diff -u $file.kept $"($generated)/($file.written)"
    }
    # makepkg sets srcdir and pkgdir, and reads the arrays itself.
    ^shellcheck --shell=bash --exclude=SC2034,SC2154,SC2164 ...(glob dist/aur/*/PKGBUILD)
    ^desktop-file-validate assets/io.github.felintusun.edualc.desktop
    ^appstreamcli validate --no-net --pedantic assets/io.github.felintusun.edualc.metainfo.xml
}

# Runs lintian on the .deb files in `folder`; warnings fail too. initial-upload-closes-no-bugs is
# suppressed: it concerns uploads to Debian's archive, which these packages don't go through.
def "main lint-deb" [folder: path] {
    (^lintian --pedantic --display-info --fail-on error,warning
        --suppress-tags initial-upload-closes-no-bugs
        ...(glob $"($folder)/*.deb"))
}

# Runs rpmlint on the .rpm files under `folder`, with dist/rpmlint.toml.
def "main lint-rpm" [folder: path] {
    ^rpmlint --config dist/rpmlint.toml ...(glob $"($folder)/**/*.rpm")
}

# Writes the -git PKGBUILD with this checkout as its source, for building the commit at hand.
def "main pkgbuild-local" []: nothing -> path {
    let generated = main generate
    let work = fresh pkgbuild
    let local = 'source=("$_name::git+file://' + (pwd) + '")'
    open --raw $"($generated)/arch/edualc-git/PKGBUILD"
    | str replace 'source=("git+$url.git")' $local
    | save $"($work)/PKGBUILD"
    $work
}

# Builds the .deb.
def "main deb" []: nothing -> path {
    # dpkg-deb clamps the files' times to it.
    $env.SOURCE_DATE_EPOCH = epoch | into string
    let generated = main generate
    let name = open --raw $"($generated)/name"
    let work = fresh deb
    let tree = $"($work)/debian/($name)"
    let docs = $"($tree)/usr/share/doc/($name)"
    mkdir $"($work)/debian" $"($tree)/DEBIAN"
    cp $"($generated)/debian/control" $"($generated)/debian/changelog" $"($work)/debian/"
    stage (open $"($generated)/debian/files.json") $tree
    ^install -Dm0644 $"($generated)/debian/copyright" $"($docs)/copyright"
    open --raw $"($generated)/debian/changelog" | ^gzip -9n | save --raw $"($docs)/changelog.Debian.gz"
    ^chmod 0644 $"($docs)/changelog.Debian.gz"
    do {
        cd $work
        ^dpkg-shlibdeps -Tdebian/substvars $"debian/($name)/usr/bin/($name)"
        ^dpkg-gencontrol $"-p($name)" $"-Pdebian/($name)" -Tdebian/substvars
    }
    do {
        cd $tree
        glob --no-dir usr/**/*
        | each {|file| $file | path relative-to $tree }
        | sort
        | each {|file| $"(open --raw $file | hash md5)  ($file)" }
        | str join "\n"
        | $in + "\n"
        | save DEBIAN/md5sums
    }
    ^chmod 0644 $"($tree)/DEBIAN/md5sums"
    let out = $"($work)/out"
    mkdir $out
    ^dpkg-deb --root-owner-group -Zxz --build $tree $out | print --stderr
    $out
}

# Builds the .rpm (in a subfolder named after the architecture).
def "main rpm" []: nothing -> path {
    $env.SOURCE_DATE_EPOCH = epoch | into string
    let generated = main generate
    let work = fresh rpm
    # The build time, file times and build host come from SOURCE_DATE_EPOCH and a fixed name.
    (^rpmbuild -bb
        --define "use_source_date_epoch_as_buildtime 1"
        --define "clamp_mtime_to_source_date_epoch 1"
        --define "_buildhost reproducible"
        --define $"_sourcedir (pwd)"
        --define $"_topdir ($work)"
        --define $"_rpmdir ($work)/out"
        $"($generated)/fedora/package.spec"
        | print --stderr)
    $"($work)/out"
}

# Builds the tarball.
def "main tarball" []: nothing -> path {
    let generated = main generate
    let name = open --raw $"($generated)/name"
    let version = open --raw $"($generated)/version"
    let folder = $"($name)-($version)-(uname | get machine)-linux"
    let work = fresh tarball
    stage (open $"($generated)/tarball/files.json") $"($work)/($folder)"
    let out = $"($work)/out"
    mkdir $out
    (^tar -C $work --sort=name --owner=0 --group=0 --numeric-owner $"--mtime=@(epoch)" -cf - $folder
        | ^gzip -9n
        | save --raw $"($out)/($folder).tar.gz")
    $out
}
