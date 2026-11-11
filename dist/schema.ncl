# What dist/package.ncl and dist/releases.json are checked against, with what each field is for:
# `nickel query dist/package.ncl --field <field>` shows both.
let matching = fun pattern failure hint =>
  std.contract.from_validator (fun value =>
    if std.is_string value && std.string.is_match pattern value then
      'Ok
    else
      'Error { message = failure, notes = [hint] }
  )
in
let numbers = fun version => std.string.split "." version |> std.array.map std.string.to_number in
# Whether version `a` comes after version `b`.
let later = fun a b =>
  let rec compare = fun left right =>
    if left == [] then
      false
    else if std.array.first left != std.array.first right then
      std.array.first left > std.array.first right
    else
      compare (std.array.drop_first left) (std.array.drop_first right)
  in
  compare (numbers a) (numbers b)
in
# A date (2026-11-10) as a number that orders as dates do (20261110).
let day = fun date => std.string.to_number (std.string.replace "-" "" date) in
let adjacent = fun list =>
  if std.array.length list < 2 then
    []
  else
    std.array.zip_with (fun first second => { newer = first, older = second }) (std.array.slice 0 (std.array.length list - 1) list) (std.array.drop_first list)
in
let unique_by = fun key list =>
  let keys = std.array.map key list in
  std.array.length keys == std.array.length (std.array.fold_left (fun kept item => if std.array.elem item kept then kept else kept @ [item]) [] keys)
in
let single_line =
  std.contract.from_validator (fun value =>
    if !(std.is_string value) || !(std.string.is_match "^[^\\s%;][^\\n\\r%;]*$" value) || std.string.is_match "\\s$" value then
      'Error {
        message = "not a single line of text",
        notes = ["It goes into control files, specs and PKGBUILDs: one line, no `%` (an RPM macro), no `;` (a desktop entry's separator), no space at either end."],
      }
    else
      'Ok
  )
in
{
  Version =
    matching
      "^(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)$"
      "not a release version (1.2.3)"
      "RPM's Version and makepkg's pkgver can't hold a `-`, and Debian would sort a pre-release after its release.",
  Date = matching "^[0-9]{4}-(0[1-9]|1[0-2])-(0[1-9]|[12][0-9]|3[01])$" "not a date (2026-11-10)" "AppStream and the changelogs take a day.",
  Sha256 = matching "^[0-9a-f]{64}$" "not a SHA-256 in hex" "As sha256sum prints it.",
  AppId =
    matching
      "^[A-Za-z_][A-Za-z0-9_]*(\\.[A-Za-z_][A-Za-z0-9_]*){2,}$"
      "not a reverse-DNS application id"
      "AppStream's rules: at least three parts, each starting with a letter or `_`, no `-`.",
  # Each segment starts with a letter, digit or `_`: no `..`, no leading `/` or `-`.
  Path =
    matching
      "^[A-Za-z0-9_][A-Za-z0-9._-]*(/[A-Za-z0-9_][A-Za-z0-9._-]*)*$"
      "not a path the packaging files can hold"
      "Paths go into shell lines and RPM specs unquoted: relative, letters, digits and `._/-`, no segment starting with `.` or `-`.",
  Soname = matching "^lib[A-Za-z0-9_+-]+\\.so\\.[0-9]+$" "not a library's soname (libfoo.so.1)" "As readelf shows it under NEEDED.",
  DebianName = matching "^[a-z0-9][a-z0-9+.-]+$" "not a Debian package name" "Debian policy 5.6.1.",
  FedoraName = matching "^[A-Za-z0-9_][A-Za-z0-9._+-]*$" "not a Fedora package name" "Letters, digits and `._+-`.",
  ArchName = matching "^[a-z0-9@_+][a-z0-9@._+-]*$" "not an Arch package name" "Lower case, digits and `@._+-`, not starting with `-` or `.`.",
  Line = single_line,
  # A summary: Debian's synopsis, RPM's Summary, AppStream's summary, the desktop entry's
  # Comment.
  Summary =
    std.contract.from_validator (fun value =>
      if std.string.length value > 80 || std.string.is_match "\\.$" value then
        'Error { message = "not a summary", notes = ["At most 80 characters (lintian), without a full stop at the end (AppStream)."] }
      else
        'Ok
    ),

  # What a shipped file is, which decides where each format puts it.
  Role = [| 'Program, 'DesktopEntry, 'Icon, 'Metainfo, 'Manual, 'Documentation, 'Notices, 'License |],
  Format = [| 'Debian, 'Fedora, 'Arch, 'Tarball |],

  File = {
    source | Path | doc "Its path in the tree.",
    role | Role | doc "What it is; formats.ncl places it by this.",
  },
  # A library the program opens at run time, which no tool finds in the binary.
  Opened = {
    soname | Soname | doc "As dlopen asks for it; Fedora's packages provide it by this name.",
    debian | DebianName | doc "The Debian package that has it.",
    arch | ArchName | doc "The Arch package that has it.",
  },
  # A library the program links, as readelf lists it. Debian and Fedora find these themselves.
  Linked = {
    soname | String | doc "As readelf lists it under NEEDED.",
    arch | ArchName | doc "The Arch package that has it; makepkg doesn't find it.",
  },
  # A package edualc needs or can use, by its name in the distributions that have it.
  Other = {
    why | Line | doc "What edualc uses it for.",
    debian | DebianName | optional,
    fedora | FedoraName | optional,
    arch | ArchName | optional,
  },
  Release = {
    version | Version,
    date | Date | doc "The day it was tagged, as the tag records it.",
    source_sha256
      | Sha256
      | optional
      | doc "The tag's source tarball as GitHub serves it, for the AUR's PKGBUILD (`just release-checksum`).",
  },
  Releases =
    std.contract.from_validator (fun releases =>
      if releases == [] then
        'Error { message = "no releases", notes = ["The first is the version in Cargo.toml."] }
      else if !(std.array.all (fun pair => later pair.newer.version pair.older.version) (adjacent releases)) then
        'Error { message = "the releases aren't newest first", notes = ["Each version is lower than the one before it."] }
      else if !(std.array.all (fun pair => day pair.newer.date >= day pair.older.date) (adjacent releases)) then
        'Error { message = "a release is dated before the one after it", notes = [] }
      else
        'Ok
    ),

  # Licences by SPDX id, with their names in Debian's copyright file and /usr/share/common-licenses.
  licences = {
    "GPL-3.0-or-later" = { debian = "GPL-3+", common = "GPL-3" },
  },
  Licence =
    std.contract.from_validator (fun value =>
      if std.is_string value && std.record.has_field value licences then
        'Ok
      else
        'Error {
          message = "a licence dist/schema.ncl doesn't know",
          notes = ["Add it to `licences` there, with its names in Debian's copyright format."],
        }
    ),

  Package = {
    name | String | doc "The program's and the packages' name, from Cargo.toml.",
    app_id | AppId | doc "The desktop entry's, icon's, AppStream and Wayland id (also in src/desktop.rs).",
    version | Version | doc "From Cargo.toml.",
    homepage | String | doc "Cargo.toml's repository.",
    license | Licence | doc "SPDX, from Cargo.toml.",
    maintainer | doc "Who the packages name as their maintainer." = {
      name | Line,
      email | String,
    },
    copyright | Line | doc "For Debian's copyright file.",
    summary | Line | Summary | doc "One line, from Cargo.toml's description.",
    description | Line | doc "A paragraph; the formats wrap it.",
    generic_name | Line | doc "The desktop entry's GenericName.",
    categories | Array Line | doc "The desktop entry's Categories (freedesktop's menu spec).",
    keywords | Array Line | doc "The desktop entry's Keywords, for finding it in a launcher.",
    files | Array File | doc "Files shipped, by path in the tree. Each format places them by role.",
    opened | Array Opened | doc "Libraries winit opens at run time (dlopen), which no tool finds.",
    linked
      | Array Linked
      | doc "Libraries the program may link; the release checks the binary against these.",
    needed | Array Other | doc "Other packages it needs, where a format doesn't find them.",
    recommended | Array Other | doc "Packages it can use, and what for.",
    releases | Array Release | Releases | doc "Every release, newest first (dist/releases.json).",
  },

  # The package as a whole, beyond each field's own contract.
  Consistent =
    std.contract.from_validator (fun package =>
      let programs = std.array.filter (fun file => file.role == 'Program) package.files in
      if std.array.length programs != 1 then
        'Error { message = "not exactly one program among the files", notes = [] }
      else if !(unique_by (fun file => file.source) package.files) then
        'Error { message = "a file is listed twice", notes = [] }
      else if (std.array.first package.releases).version != package.version then
        'Error {
          message = "the newest release isn't version %{package.version}",
          notes = ["Cargo.toml's version comes first in dist/releases.json, with the day it's tagged."],
        }
      else
        'Ok
    ),
}
