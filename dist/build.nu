#!/usr/bin/env nu

const root = path self ..
const packaging = "target/packaging"
# Generated files kept in the tree: path in the tree → path in `generate`'s folder. The release's
# PKGBUILD builds the last tag's tree, so `write-release` writes it, in that tree.
const kept = {
    "assets/io.github.felintusun.edualc.desktop": "desktop"
    "assets/io.github.felintusun.edualc.metainfo.xml": "metainfo.xml"
    "dist/aur/edualc-git/PKGBUILD": "arch/edualc-git/PKGBUILD"
}

# Packages edualc with each format's own tools, from dist/package.ncl through dist/formats.ncl.
#
# Commands that build write into a new folder under target/packaging and print its path; the
# tools' own output goes to stderr. The packages' dates are the newest release's day in
# dist/releases.json, or SOURCE_DATE_EPOCH. deb, rpm and tarball package target/release/edualc and edualcd.
def main []: nothing -> nothing {
    help main
}

def --env at-root []: nothing -> nothing {
    cd $root
}

# A fresh folder for one run of `kind`. (An absolute path: mktemp takes a relative one from where
# the script started, not from `cd`.)
def fresh [kind: string]: nothing -> path {
    let base = $root | path join $packaging
    mkdir $base
    mktemp --directory --tmpdir-path $base $"($kind).XXXXXX"
}

# The packages' time, in seconds since 1970: SOURCE_DATE_EPOCH, else midnight (UTC) of `date`.
def epoch [date: string]: nothing -> int {
    if ($env.SOURCE_DATE_EPOCH? | is-empty) {
        ($"($date)T00:00:00+00:00" | into datetime | into int) // 1_000_000_000
    } else {
        $env.SOURCE_DATE_EPOCH | into int
    }
}

# The files of `pattern`, or an error naming `what` is missing.
def found [pattern: string, what: string]: nothing -> list<string> {
    let files = glob $pattern
    if ($files | is-empty) {
        error make { msg: $"no ($what) at ($pattern)" }
    }
    $files
}

# Installs `files` under `tree`, with every folder 0755 whatever the umask.
def stage [
    files: list<record<source: string, role: string, path: string, mode: string, gzip: bool>>
    tree: path
]: nothing -> nothing {
    for file in $files {
        let target = $tree | path join ($file.path | str trim --left --char "/")
        mkdir ($target | path dirname)
        if $file.gzip {
            open --raw $file.source | ^gzip -9n | save --raw $target
        } else {
            open --raw $file.source | save --raw $target
        }
        ^chmod $file.mode $target
    }
    ^find $tree -type d -exec chmod 0755 "{}" "+"
}

# Writes the packaging files from dist/*.ncl, and formats.json with everything formats.ncl says.
def "main generate" []: nothing -> path {
    at-root
    let generated = fresh generated
    let formats = ^nickel export --format json dist/formats.ncl | from json
    $formats | to json | save $"($generated)/formats.json"
    mkdir $"($generated)/debian" $"($generated)/fedora" $"($generated)/arch/edualc" $"($generated)/arch/edualc-git"
    for part in [control changelog copyright] {
        $formats.debian | get $part | save $"($generated)/debian/($part)"
    }
    $formats.fedora.spec | save $"($generated)/fedora/package.spec"
    $formats.arch.git | save $"($generated)/arch/edualc-git/PKGBUILD"
    if $formats.arch.tagged != null {
        $formats.arch.tagged | save $"($generated)/arch/edualc/PKGBUILD"
    }
    $formats.desktop | save $"($generated)/desktop"
    [
        '<?xml version="1.0" encoding="UTF-8"?>'
        '<!-- Written from dist/package.ncl by dist/formats.ncl and dist/build.nu: change those, not this. -->'
        ($formats.metainfo | to xml --indent 2 --self-closed --partial-escape)
    ]
    | str join "\n"
    | $in + "\n"
    | save $"($generated)/metainfo.xml"
    $generated
}

# Writes the generated files kept in the tree.
def "main write-kept" []: nothing -> nothing {
    at-root
    let generated = main generate
    for file in ($kept | transpose kept written) {
        cp $"($generated)/($file.written)" $file.kept
    }
}

# After tagging v<Cargo.toml's version> and pushing the tag: puts the tag's tarball's checksum,
# as GitHub serves it, into dist/releases.json, once its contents match `git archive` of the tag,
# then writes the release's PKGBUILD.
def "main release-checksum" []: nothing -> nothing {
    at-root
    let package = open Cargo.toml | get workspace.package | insert name "edualc"
    let releases = open dist/releases.json
    if ($releases | first | get version) != $package.version {
        error make { msg: $"dist/releases.json doesn't start with ($package.version)" }
    }
    let tag = $"v($package.version)"
    let url = $"($package.repository)/archive/refs/tags/($tag).tar.gz"
    let served = http get --raw $url
    let served_tar = $served | ^gzip -dc | hash sha256
    let archived_tar = ^git archive --format=tar $"--prefix=($package.name)-($package.version)/" $tag | hash sha256
    if $served_tar != $archived_tar {
        error make { msg: $"($url) doesn't hold `git archive` of ($tag)" }
    }
    $releases
    | update 0 {|release| $release | upsert source_sha256 ($served | hash sha256) }
    | to json --indent 2
    | $in + "\n"
    | save --force dist/releases.json
    main write-release
}

# Writes the release's PKGBUILD. Run in the tagged tree, once the release's checksum is in
# dist/releases.json.
def "main write-release" []: nothing -> nothing {
    at-root
    let generated = main generate
    let written = $"($generated)/arch/edualc/PKGBUILD"
    if not ($written | path exists) {
        error make { msg: "no checksum for the newest release in dist/releases.json: see `release-checksum`" }
    }
    cp $written dist/aur/edualc/PKGBUILD
}

# Checks that every v* tag in HEAD's history is in dist/releases.json, with the tag's day.
def "main check-tags" []: nothing -> nothing {
    at-root
    let releases = open dist/releases.json
    let tags = (^git for-each-ref --merged HEAD --format "%(refname:short) %(creatordate:short)" "refs/tags/v*"
        | lines
        | parse "v{version} {date}")
    for tag in $tags {
        let listed = $releases | where version == $tag.version
        if ($listed | is-empty) {
            error make { msg: $"v($tag.version) isn't in dist/releases.json" }
        }
        if ($listed | first | get date) != $tag.date {
            error make { msg: $"v($tag.version) was tagged on ($tag.date), not as dist/releases.json says" }
        }
    }
}

# Checks the Nickel files' formatting, types and contracts, the kept files against freshly
# generated ones, the release tags, and lints the PKGBUILDs, the desktop entry and the metainfo.
def "main check" []: nothing -> nothing {
    at-root
    ^nickel format --check ...(glob dist/*.ncl)
    for file in (glob dist/*.ncl) {
        ^nickel typecheck $file
    }
    # Exporting everything applies every contract.
    let generated = main generate
    for file in ($kept | transpose kept written) {
        ^diff -u $file.kept $"($generated)/($file.written)"
    }
    main check-tags
    # makepkg sets srcdir and pkgdir, and reads the arrays itself.
    (^shellcheck --shell=bash --exclude=SC2034,SC2154,SC2164
        ...(glob $"($generated)/arch/*/PKGBUILD") ...(glob dist/aur/*/PKGBUILD))
    ^desktop-file-validate assets/io.github.felintusun.edualc.desktop
    ^appstreamcli validate --no-net --pedantic assets/io.github.felintusun.edualc.metainfo.xml
}

# Checks that each of `programs` links only the libraries dist/package.ncl lists under `linked`.
def "main check-linked" [...programs: path]: nothing -> nothing {
    at-root
    let programs = if ($programs | is-empty) { ["target/release/edualc", "target/release/edualcd"] } else { $programs }
    let linked = (main generate | path join formats.json | open $in | get linked)
    for program in $programs {
        let needed = ^readelf -d $program | lines | parse --regex '\(NEEDED\)\s+Shared library: \[(?<soname>[^\]]+)\]' | get soname
        print --stderr ...$needed
        let unlisted = $needed | where $it not-in $linked
        if ($unlisted | is-not-empty) {
            error make { msg: $"($program) links ($unlisted | str join ', '), which dist/package.ncl doesn't list" }
        }
    }
}

# Runs lintian on the .deb files in `folder`; warnings fail too. initial-upload-closes-no-bugs is
# suppressed: it concerns uploads to Debian's archive, which these packages don't go through.
def "main lint-deb" [folder: path]: nothing -> nothing {
    (^lintian --pedantic --display-info --fail-on error,warning
        --suppress-tags initial-upload-closes-no-bugs
        ...(found $"($folder)/*.deb" ".deb files"))
}

# Runs rpmlint on the .rpm files under `folder`, with dist/rpmlint.toml; warnings fail too.
def "main lint-rpm" [folder: path]: nothing -> nothing {
    ^rpmlint --strict --config $"($root)/dist/rpmlint.toml" ...(found $"($folder)/**/*.rpm" ".rpm files")
}

# Runs namcap on the packages in `folder`. Its errors and warnings fail, except those
# dist/formats.ncl expects (`arch.namcap_expected`).
def "main lint-arch" [folder: path]: nothing -> nothing {
    let packages = found $"($folder)/*.pkg.tar.zst" "Arch packages"
    let expected = (main generate | path join formats.json | open $in | get arch.namcap_expected)
    let messages = ^namcap ...$packages | lines
    print --stderr ...$messages
    let unexpected = ($messages
        | where $it =~ ' [EW]: '
        | where {|message| $expected | all {|pattern| $message !~ $pattern } })
    if ($unexpected | is-not-empty) {
        error make { msg: $"namcap: ($unexpected | str join '; ')" }
    }
}

# Writes the -git PKGBUILD with this checkout as its source, for building the commit at hand.
def "main pkgbuild-local" []: nothing -> path {
    at-root
    let generated = main generate
    let work = fresh pkgbuild
    let written = open --raw $"($generated)/arch/edualc-git/PKGBUILD"
    let local = $written | str replace 'source=("git+$url.git")' ('source=("$_name::git+file://' + $root + '")')
    if $local == $written {
        error make { msg: "the -git PKGBUILD has no source=(\"git+$url.git\") line to replace" }
    }
    $local | save $"($work)/PKGBUILD"
    $work
}

# Builds the .deb from target/release/edualc.
def "main deb" []: nothing -> path {
    at-root
    let generated = main generate
    let formats = open $"($generated)/formats.json"
    let name = $formats.name
    # dpkg-deb clamps the files' times to it.
    $env.SOURCE_DATE_EPOCH = epoch $formats.date | into string
    let work = fresh deb
    let tree = $"($work)/debian/($name)"
    let docs = $"($tree)/usr/share/doc/($name)"
    mkdir $"($work)/debian" $"($tree)/DEBIAN"
    cp $"($generated)/debian/control" $"($generated)/debian/changelog" $"($work)/debian/"
    stage $formats.debian.files $tree
    open --raw $"($generated)/debian/copyright" | save --raw $"($docs)/copyright"
    open --raw $"($generated)/debian/changelog" | ^gzip -9n | save --raw $"($docs)/changelog.Debian.gz"
    ^chmod 0644 $"($docs)/copyright" $"($docs)/changelog.Debian.gz"
    do {
        cd $work
        ^dpkg-shlibdeps -Tdebian/substvars ...($formats.programs | each {|program| $"debian/($name)($program)" })
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
    ^chmod 0755 $"($tree)/DEBIAN"
    let out = $"($work)/out"
    mkdir $out
    ^dpkg-deb --root-owner-group -Zxz --build $tree $out | print --stderr
    $out
}

# Builds the .rpm from target/release/edualc, in a subfolder named after the architecture.
def "main rpm" []: nothing -> path {
    at-root
    let generated = main generate
    let formats = open $"($generated)/formats.json"
    $env.SOURCE_DATE_EPOCH = epoch $formats.date | into string
    let work = fresh rpm
    # The build time, file times and build host come from SOURCE_DATE_EPOCH and a fixed name.
    (^rpmbuild -bb
        --define "use_source_date_epoch_as_buildtime 1"
        --define "clamp_mtime_to_source_date_epoch 1"
        --define "_buildhost reproducible"
        --define $"_sourcedir ($root)"
        --define $"_topdir ($work)"
        --define $"_rpmdir ($work)/out"
        $"($generated)/fedora/package.spec"
        | print --stderr)
    $"($work)/out"
}

# Builds the tarball from target/release/edualc: one folder, named after the machine.
def "main tarball" []: nothing -> path {
    at-root
    let generated = main generate
    let formats = open $"($generated)/formats.json"
    let folder = $"($formats.name)-($formats.version)-(uname | get machine)-linux"
    let work = fresh tarball
    stage $formats.tarball.files $"($work)/($folder)"
    let out = $"($work)/out"
    mkdir $out
    (^tar -C $work --sort=name --owner=0 --group=0 --numeric-owner $"--mtime=@(epoch $formats.date)" -cf - $folder
        | ^gzip -9n
        | save --raw $"($out)/($folder).tar.gz")
    $out
}
