# Packaging files per format, from package.ncl: the desktop entry, the AppStream metainfo,
# Debian's control files, Fedora's spec, the AUR's PKGBUILDs and the tarball's file list.
# dist/build.nu exports this once, as JSON, and writes the files.
#
# The helpers are statically typed. Their enum types are written out (a named type is a contract
# to the typechecker), so a match that misses a role or format doesn't typecheck.
let schema = import "schema.ncl" in
let p = import "package.ncl" in

let basename : String -> String = fun path => std.array.last (std.string.split "/" path) in
let lines : Array String -> String = fun list => std.string.join "\n" list in
let unique : Array String -> Array String = fun list =>
  std.array.fold_left (fun kept item => if std.array.any (fun other => other == item) kept then kept else kept @ [item]) [] list
in
# A shell word holding `text` as it is.
let quoted : String -> String = fun text => "'%{std.string.replace "'" "'\\''" text}'" in
# `text` broken into lines of at most `width` characters, between words.
let wrapped : Number -> String -> Array String = fun width text =>
  let step = fun state word =>
    if state.line == "" then
      { done = state.done, line = word }
    else if std.string.length state.line + 1 + std.string.length word > width then
      { done = state.done @ [state.line], line = word }
    else
      { done = state.done, line = "%{state.line} %{word}" }
  in
  let last = std.array.fold_left step { done = [], line = "" } (std.string.split " " text) in
  last.done @ [last.line]
in
# The names `list`'s entries have in `distribution`, for those that have one.
let named = fun distribution list =>
  list
  |> std.array.filter (fun entry => std.record.has_field distribution entry)
  |> std.array.map (fun entry => std.record.get distribution entry)
  |> unique
in

# Dates as the changelogs write them, from a release's day.
let weekdays : Array String = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"] in
let months : Array String = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"] in
let dated : String -> { weekday : String, day : String, month : String, year : String } = fun date =>
  let parts = std.string.split "-" date in
  # (Named apart from the fields below, which would otherwise refer to themselves.)
  let year_number = std.string.to_number (std.array.at 0 parts) in
  let month_number = std.string.to_number (std.array.at 1 parts) in
  let day_number = std.string.to_number (std.array.at 2 parts) in
  # Sakamoto's method; 0 is Sunday.
  let shifted = if month_number < 3 then year_number - 1 else year_number in
  let div = fun a b => std.number.floor (a / b) in
  let offset = std.array.at (month_number - 1) [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4] in
  let weekday_number = (shifted + div shifted 4 - div shifted 100 + div shifted 400 + offset + day_number) % 7 in
  {
    weekday = std.array.at weekday_number weekdays,
    day = std.array.at 2 parts,
    month = std.array.at (month_number - 1) months,
    year = std.array.at 0 parts,
  }
in
let rfc2822 = fun date => let d = dated date in "%{d.weekday}, %{d.day} %{d.month} %{d.year} 00:00:00 +0000" in
let rpm_date = fun date => let d = dated date in "%{d.weekday} %{d.month} %{d.day} %{d.year}" in

let doc_dir = "/usr/share/doc/%{p.name}" in
let licenses_dir = "/usr/share/licenses/%{p.name}" in
# Where each role's files go in `format`'s package, or that it doesn't ship them.
let folder
  : [| 'Debian, 'Fedora, 'Arch, 'Tarball |] -> [| 'Program, 'DesktopEntry, 'Icon, 'Metainfo, 'Manual, 'Documentation, 'Notices, 'License |] -> [| 'Into String, 'Left |]
  = fun format role =>
    let usual = fun dir =>
      role
      |> match {
        'Program => 'Into "/usr/bin",
        'DesktopEntry => 'Into "/usr/share/applications",
        'Icon => 'Into "/usr/share/icons/hicolor/scalable/apps",
        'Metainfo => 'Into "/usr/share/metainfo",
        'Manual => 'Into "/usr/share/man/man1",
        'Documentation => 'Into doc_dir,
        'Notices => 'Into dir.notices,
        'License => dir.license,
      }
    in
    format
    |> match {
      # Debian keeps notices with the documentation, and the licence in its copyright file.
      'Debian => usual { notices = doc_dir, license = 'Left },
      'Fedora => usual { notices = licenses_dir, license = 'Into licenses_dir },
      # Arch ships the GPL in its licenses package.
      'Arch => usual { notices = licenses_dir, license = 'Left },
      # One folder, everything in it.
      'Tarball => 'Into "",
    }
in
let mode_of
  : [| 'Program, 'DesktopEntry, 'Icon, 'Metainfo, 'Manual, 'Documentation, 'Notices, 'License |] -> String
  = fun role =>
    role
    |> match {
      'Program => "0755",
      'DesktopEntry => "0644",
      'Icon => "0644",
      'Metainfo => "0644",
      'Manual => "0644",
      'Documentation => "0644",
      'Notices => "0644",
      'License => "0644",
    }
in
# The files `format` ships, each with where it goes, its mode, and whether to gzip it (Debian's
# manual pages; rpmbuild and makepkg compress theirs).
let staged
  : [| 'Debian, 'Fedora, 'Arch, 'Tarball |] -> Array { source : String, role : [| 'Program, 'DesktopEntry, 'Icon, 'Metainfo, 'Manual, 'Documentation, 'Notices, 'License |] } -> Array { source : String, role : [| 'Program, 'DesktopEntry, 'Icon, 'Metainfo, 'Manual, 'Documentation, 'Notices, 'License |], path : String, mode : String, gzip : Bool }
  = fun format files =>
    files
    |> std.array.filter_map (fun file =>
      folder format file.role
      |> match {
        'Into dir =>
          let compressed = format == 'Debian && file.role == 'Manual in
          let placed = "%{dir}/%{basename file.source}" in
          'Some {
            source = file.source,
            role = file.role,
            path = if compressed then placed ++ ".gz" else placed,
            mode = mode_of file.role,
            gzip = compressed,
          },
        'Left => 'None,
      }
    )
in
let shipped = fun format => staged format p.files in
# Lines installing the files `format` ships from under `from` into the tree under `into`.
let install = fun format from into =>
  shipped format
  |> std.array.map (fun file => "install -Dm%{file.mode} %{from}%{file.source} %{into}%{file.path}")
  |> lines
in

let newest = std.array.first p.releases in
let licence = std.record.get p.license schema.licences in
let maintainer = "%{p.maintainer.name} <%{p.maintainer.email}>" in
let release_url = fun version => "%{p.homepage}/releases/tag/v%{version}" in
let debian_programs = std.array.filter (fun file => file.role == 'Program) (shipped 'Debian) in
let debian_notices = std.array.first (std.array.filter (fun file => file.role == 'Notices) (shipped 'Debian)) in
# An XML element in the shape Nushell's `to xml` takes.
let element = fun name attrs children => { tag = name, attributes = attrs, content = children } in
let text_element = fun tag text => element tag {} [text] in

{
  name = p.name,
  version = p.version,
  # The newest release's day: the changelogs' date, and the packages' files' times.
  date = newest.date,
  # Where the programs are in the Debian package's tree, for dpkg-shlibdeps.
  programs = std.array.map (fun file => file.path) debian_programs,
  # What the programs may link (dist/build.nu's `check-linked`).
  linked = std.array.map (fun library => library.soname) p.linked,

  desktop =
    m%"
      [Desktop Entry]
      Type=Application
      Name=%{p.name}
      GenericName=%{p.generic_name}
      Comment=%{p.summary}
      Exec=%{p.name} %f
      Icon=%{p.app_id}
      Terminal=false
      Categories=%{std.string.join ";" p.categories};
      Keywords=%{std.string.join ";" p.keywords};
      StartupWMClass=%{p.app_id}
    "%
    ++ "\n",

  # The AppStream component, as Nushell's `to xml` takes it.
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
        element "releases" {} (std.array.map (fun release => element "release" { version = release.version, date = release.date } []) p.releases),
      ],

  debian = {
    # Source-package form for dpkg-gencontrol, which adds the architecture, the installed size
    # and `shlibs:Depends` from dpkg-shlibdeps.
    control =
      let recommends = named "debian" p.recommended in
      m%"
        Source: %{p.name}
        Section: devel
        Priority: optional
        Maintainer: %{maintainer}
        Homepage: %{p.homepage}
        Rules-Requires-Root: no

        Package: %{p.name}
        Architecture: any
        Depends: %{std.string.join ", " (["${shlibs:Depends}"] @ named "debian" p.opened @ named "debian" p.needed)}
        %{if recommends == [] then "" else "Recommends: " ++ std.string.join ", " recommends ++ "\n"}Description: %{p.summary}
         %{lines (wrapped 78 p.description)}
      "%
      ++ "\n",
    changelog =
      p.releases
      |> std.array.map (fun release =>
        m%"
          %{p.name} (%{release.version}-1) unstable; urgency=medium

            * Release %{release.version}: %{release_url release.version}

           -- %{maintainer}  %{rfc2822 release.date}
        "%
      )
      |> std.string.join "\n\n"
      |> (fun text => text ++ "\n"),
    copyright =
      m%"
        Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
        Upstream-Name: %{p.name}
        Source: %{p.homepage}

        Files: *
        Copyright: %{p.copyright}
        License: %{licence.debian}
         On Debian systems, the full text of the license can be found in
         /usr/share/common-licenses/%{licence.common}. The licenses of the crates
         built into the program are in %{debian_notices.path}.
      "%
      ++ "\n",
    files = shipped 'Debian,
  },

  # RPM's macros are written `%{…}` as Nickel's interpolations are, so here Nickel's are `%%{…}`.
  # The manual page's path ends in `*` because rpmbuild compresses it.
  fedora.spec =
    let macro = fun name => "%" ++ "{" ++ name ++ "}" in
    let files = shipped 'Fedora in
    # The package's own folders, which it owns so that removing it removes them.
    let owned = files |> std.array.map (fun file => std.string.replace "/%{basename file.path}" "" file.path) |> std.array.filter (fun dir => std.string.is_match "/%{p.name}$" dir) |> unique in
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
      %%{lines (std.array.map (fun name => "Requires:       " ++ name) (named "fedora" p.needed))}
      %%{lines (std.array.map (fun name => "Recommends:     " ++ name) (named "fedora" p.recommended))}

      %description
      %%{lines (wrapped 78 p.description)}

      %install
      %%{install 'Fedora (macro "_sourcedir" ++ "/") (macro "buildroot")}

      %files
      %%{lines (std.array.map (fun dir => "%dir " ++ dir) owned)}
      %%{lines (std.array.map (fun file =>
          file.role |> match {
            'Documentation => "%doc " ++ file.path,
            'Notices => "%license " ++ file.path,
            'License => "%license " ++ file.path,
            'Manual => file.path ++ "*",
            'Program => file.path,
            'DesktopEntry => file.path,
            'Icon => file.path,
            'Metainfo => file.path,
          }) files)}

      %changelog
      %%{std.string.join "\n\n" (std.array.map (fun release => "* " ++ rpm_date release.date ++ " " ++ maintainer ++ " - " ++ release.version ++ "-1\n- Release " ++ release.version ++ ": " ++ release_url release.version) p.releases)}
    "%%
    ++ "\n",

  tarball.files = shipped 'Tarball,

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
        license=(%{quoted p.license})
        # Cargo strips the binary (strip = true), which leaves a debug package nothing to hold.
        options=('!debug')
        depends=(%{std.string.join " " (unique (named "arch" p.linked @ named "arch" p.opened @ named "arch" p.needed))})
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
      # The PKGBUILD of the release in Cargo.toml, once its tarball's checksum is in
      # releases.json (`just release-checksum`); null before.
      tagged =
        if std.record.has_field "source_sha256" newest then
          header { pkgname = p.name, name_line = "", desc_suffix = "", makedepends = ["cargo"] }
          ++ "\n"
          ++ m%"
            source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/v$pkgver.tar.gz")
            sha256sums=('%{newest.source_sha256}')
          "%
          ++ "\n\n"
          ++ body "$pkgname-$pkgver"
          ++ "\n"
        else
          null,
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
      # namcap's messages that are expected, as patterns: it can't see the libraries opened at
      # run time used, and lists the dynamic loader as unused.
      namcap_expected =
        (named "arch" p.opened |> std.array.map (fun name => "Dependency included, but may not be needed \\('%{name}'\\)"))
        @ (std.array.map
            (fun file => "Unused shared library '/usr/lib(64)?/ld-linux-[a-z0-9_-]+\\.so\\.[0-9]' by file \\('%{std.string.substring 1 (std.string.length file.path) file.path}'\\)")
            (std.array.filter (fun file => file.role == 'Program) (shipped 'Arch))),
    },
}
