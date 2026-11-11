# Packaging files per format, from package.ncl: Debian's control files, Fedora's spec, the AUR's
# PKGBUILDs, the tarball's file list and the AppStream metainfo. dist/build.nu exports them one
# field at a time, merging in release.ncl and the release dates.
let p = import "package.ncl" in
let Format = [| 'Debian, 'Fedora, 'Arch, 'Tarball |] in

let basename : String -> String = fun path => std.array.last (std.string.split "/" path) in
let lines : Array String -> String = fun list => std.string.join "\n" list in
let unique : Array String -> Array String = fun list =>
  std.array.fold_left (fun kept item => if std.array.any (fun other => other == item) kept then kept else kept @ [item]) [] list
in
# A shell word holding `text` as it is.
let quoted : String -> String = fun text => "'%{std.string.replace "'" "'\\''" text}'" in
# `text` broken into lines of at most `width` characters, between words.
let wrapped : Number -> String -> Array String = fun width text =>
  let words = std.string.split " " text in
  let step = fun state word =>
    if state.line == "" then
      { done = state.done, line = word }
    else if std.string.length state.line + 1 + std.string.length word > width then
      { done = state.done @ [state.line], line = word }
    else
      { done = state.done, line = "%{state.line} %{word}" }
  in
  let last = std.array.fold_left step { done = [], line = "" } words in
  last.done @ [last.line]
in

# Where each role's files go in a format's package, or that it doesn't ship them.
let folder | Format -> p.Role -> [| 'Into String, 'Left |] = fun format role =>
  role
  |> match {
    'Program => 'Into "/usr/bin",
    'DesktopEntry => 'Into "/usr/share/applications",
    'Icon => 'Into "/usr/share/icons/hicolor/scalable/apps",
    'Metainfo => 'Into "/usr/share/metainfo",
    'Manual => 'Into "/usr/share/man/man1",
    'Documentation => 'Into "/usr/share/doc/%{p.name}",
    'Notices =>
      format
      |> match {
        'Debian => 'Into "/usr/share/doc/%{p.name}",
        _ => 'Into "/usr/share/licenses/%{p.name}",
      },
    # Debian has its copyright file; Arch ships the GPL in its licenses package.
    'License =>
      format
      |> match {
        'Fedora => 'Into "/usr/share/licenses/%{p.name}",
        _ => 'Left,
      },
  }
in
# (Not `mode`: a record field `mode = mode …` would name itself.)
let mode_of | p.Role -> String = fun role => if role == 'Program then "0755" else "0644" in
# The files `format` ships, each with where it goes.
let shipped | Format -> Array Dyn = fun format =>
  p.files
  |> std.array.filter_map (fun file =>
    folder format file.role
    |> match {
      'Into dir => 'Some (file & { path = "%{dir}/%{basename file.source}" }),
      'Left => 'None,
    }
  )
in
# The files `format` ships, for dist/build.nu to install: source, path in the tree, mode, and
# whether to gzip it (Debian's manual pages; rpmbuild and makepkg compress theirs).
let staged
  | Format -> Array { source | String, path | String, mode | String, gzip | Bool }
  = fun format =>
    shipped format
    |> std.array.map (fun file =>
      let compressed = format == 'Debian && file.role == 'Manual in
      {
        source = file.source,
        path = if compressed then file.path ++ ".gz" else file.path,
        mode = mode_of file.role,
        gzip = compressed,
      }
    )
in
# Lines installing the files `format` ships from under `from` into the tree under `into`.
let install | Format -> String -> String -> String = fun format from into =>
  shipped format
  |> std.array.map (fun file => "install -Dm%{mode_of file.role} %{from}%{file.source} %{into}%{file.path}")
  |> lines
in
let named = fun distribution list =>
  list
  |> std.array.filter (fun entry => std.record.has_field distribution entry)
  |> std.array.map (fun entry => std.record.get distribution entry)
  |> unique
in
let debian_licenses = { "GPL-3.0-or-later" = { name = "GPL-3+", text = "GPL-3" } } in
let debian_license = std.record.get p.license debian_licenses in
let maintainer = "%{p.maintainer.name} <%{p.maintainer.email}>" in
let release_url = "%{p.homepage}/releases/tag/v%{p.version}" in
# An XML element in the shape Nushell's `to xml` takes.
let element = fun name attrs children => { tag = name, attributes = attrs, content = children } in
let text_element = fun tag text => element tag {} [text] in
# The releases, newest first; the first entry has Cargo.toml's version.
let History =
  std.contract.from_validator (fun releases =>
    if std.is_array releases && std.array.length releases > 0 && (std.array.first releases).version == p.version then
      'Ok
    else
      'Error {
        message = "the release history doesn't start with version %{p.version}",
        notes = ["dist/build.nu writes it from the git tags and Cargo.toml's version."],
      }
  )
in

{
  release
    | {
      # The tag's source tarball as GitHub serves it, for the AUR's PKGBUILD (release.ncl).
      source_sha256 | String,
      # Every release, newest first, from the git tags (dist/build.nu).
      history | Array { version | String, date | String } | History,
      # The release date as Debian's and RPM's changelogs write it (dist/build.nu).
      date_rfc2822 | String,
      date_rpm | String,
    },

  # The AppStream component, as Nushell's `to xml` takes it; dist/build.nu renders it into
  # assets/.
  metainfo =
    element
      "component"
      { type = "desktop-application" }
      [
        text_element "id" p.app_id,
        text_element "metadata_license" "CC0-1.0",
        text_element "project_license" p.license,
        text_element "name" p.name,
        text_element "summary" p.summary,
        element "developer" { id = "io.github.felintusun" } [text_element "name" p.maintainer.name],
        element "description" {} [text_element "p" p.description],
        element "launchable" { type = "desktop-id" } ["%{p.app_id}.desktop"],
        element "url" { type = "homepage" } [p.homepage],
        element "url" { type = "bugtracker" } ["%{p.homepage}/issues"],
        element "url" { type = "vcs-browser" } [p.homepage],
        element "content_rating" { type = "oars-1.1" } [],
        element "provides" {} [text_element "binary" p.name],
        element
          "releases"
          {}
          (
            std.array.map (fun entry => element "release" { version = entry.version, date = entry.date } []) release.history
          ),
      ],

  debian = {
    # Source-package form for dpkg-gencontrol, which adds the architecture, the installed size
    # and `shlibs:Depends` from dpkg-shlibdeps.
    control =
      m%"
        Source: %{p.name}
        Section: devel
        Priority: optional
        Maintainer: %{maintainer}
        Homepage: %{p.homepage}
        Rules-Requires-Root: no
        Standards-Version: 4.7.0

        Package: %{p.name}
        Architecture: any
        Depends: ${shlibs:Depends}, %{std.string.join ", " (named "debian" p.opened)}
        Recommends: %{std.string.join ", " (named "debian" p.recommended)}
        Description: %{p.summary}
         %{lines (wrapped 78 p.description)}
      "%
      ++ "\n",
    changelog =
      m%"
        %{p.name} (%{p.version}-1) unstable; urgency=medium

          * Release %{p.version}: %{release_url}

         -- %{maintainer}  %{release.date_rfc2822}
      "%
      ++ "\n",
    copyright =
      m%"
        Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
        Upstream-Name: %{p.name}
        Source: %{p.homepage}

        Files: *
        Copyright: %{p.copyright}
        License: %{debian_license.name}
         On Debian systems, the full text of the license can be found in
         /usr/share/common-licenses/%{debian_license.text}. The licenses of the crates
         built into the program are in /usr/share/doc/%{p.name}/THIRD_PARTY_LICENSES.md.
      "%
      ++ "\n",
    # Files to install into the package tree: source, path, mode.
    files = staged 'Debian,
  },

  # RPM's macros are written `%{…}` as Nickel's interpolations are, so here Nickel's are `%%{…}`.
  # The manual page's path ends in `*` because rpmbuild compresses it.
  fedora.spec =
    let macro = fun name => "%" ++ "{" ++ name ++ "}" in
    m%%"
      # Packages the prebuilt program (see .github/workflows/release.yml).
      %global debug_package %{nil}

      Name:           %%{p.name}
      Version:        %%{p.version}
      Release:        1
      Summary:        %%{p.summary}
      License:        %%{p.license}
      URL:            %%{p.homepage}
      %%{lines (std.array.map (fun library => "Requires:       " ++ library.soname ++ "()(64bit)") p.opened)}
      %%{lines (std.array.map (fun name => "Recommends:     " ++ name) (named "fedora" p.recommended))}

      %description
      %%{lines (wrapped 78 p.description)}

      %install
      %%{install 'Fedora (macro "_sourcedir" ++ "/") (macro "buildroot")}

      %files
      %%{lines (std.array.map (fun file =>
          file.role |> match {
            'Documentation => "%doc " ++ file.path,
            'Notices => "%license " ++ file.path,
            'License => "%license " ++ file.path,
            'Manual => file.path ++ "*",
            _ => file.path,
          }) (shipped 'Fedora))}

      %changelog
      * %%{release.date_rpm} %%{maintainer} - %%{p.version}-1
      - Release %%{p.version}: %%{release_url}
    "%%
    ++ "\n",

  # Files in the tarball's folder: source, path, mode.
  tarball.files =
    p.files
    |> std.array.map (fun file => { source = file.source, path = basename file.source, mode = mode_of file.role, gzip = false }),

  arch =
    let header = fun variant =>
      m%"
        # Maintainer: %{maintainer}
        # Written from dist/package.ncl by dist/formats.ncl: change those, not this.

        pkgname=%{variant.pkgname}%{variant.name_line}
        pkgver=%{p.version}
        pkgrel=1
        pkgdesc=%{quoted (p.summary ++ variant.desc_suffix)}
        arch=(x86_64 aarch64)
        url=%{quoted p.homepage}
        license=(%{p.license})
        # Cargo strips the binary (strip = true), which leaves a debug package nothing to hold.
        options=('!debug')
        depends=(%{std.string.join " " (unique (p.needed_on_arch @ named "arch" p.opened))})
        makedepends=(%{std.string.join " " variant.makedepends})
        optdepends=(
          %{lines (p.recommended
            |> std.array.filter (fun entry => std.record.has_field "arch" entry)
            |> std.array.map (fun entry => quoted "%{entry.arch}: %{entry.why}"))}
        )
      "%
    in
    let body = fun dir =>
      m%"
        prepare() {
          cd "%{dir}"
          export RUSTUP_TOOLCHAIN=stable
          cargo fetch --locked --target "$(rustc --print host-tuple)"
        }

        build() {
          cd "%{dir}"
          export RUSTUP_TOOLCHAIN=stable
          export CARGO_TARGET_DIR=target
          cargo build --frozen --release
        }

        check() {
          cd "%{dir}"
          export RUSTUP_TOOLCHAIN=stable
          cargo test --frozen
        }

        package() {
          cd "%{dir}"
          %{install 'Arch "" "\"$pkgdir\""}
        }
      "%
    in
    {
      # The tagged release's PKGBUILD. Named `tagged`: `release` would shadow the record above.
      tagged =
        header { pkgname = p.name, name_line = "", desc_suffix = "", makedepends = ["cargo"] }
        ++ "\n"
        ++ m%"
          source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/v$pkgver.tar.gz")
          sha256sums=('%{release.source_sha256}')
        "%
        ++ "\n\n"
        ++ body "$pkgname-$pkgver"
        ++ "\n",
      git =
        header {
          pkgname = "%{p.name}-git",
          name_line = "\n_name=%{p.name}",
          desc_suffix = " (main branch)",
          makedepends = ["cargo", "git"],
        }
        ++ "\n"
        ++ m%"
          provides=("$_name")
          conflicts=("$_name")
          source=("git+$url.git")
          sha256sums=('SKIP')

          pkgver() {
            cd "$_name"
            local version
            version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
            printf '%s.r%s.g%s' "$version" "$(git rev-list --count HEAD)" "$(git rev-parse --short=7 HEAD)"
          }
        "%
        ++ "\n\n"
        ++ body "$_name"
        ++ "\n",
    },
}
