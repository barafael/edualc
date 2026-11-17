#!/usr/bin/env nu

# Starting edualc from this checkout while working on it: `just run`, `just try` and `just log`
# call these.

const root = path self ..
# Where `try` keeps what is its own: settings under the checkout, and a runtime folder of its own
# (under the user's) for its daemon's socket and log.
const apart_config = "target/apart/config"
const apart_runtime = "edualc-apart"

def main []: nothing -> nothing {
    help main
}

# Builds the window and the daemon it starts, then starts the window. The window starts edualcd
# from the folder it is in, so both are built every time; a daemon left running from an earlier
# build is reported, as the window joins it.
#
# --release builds them optimised. --apart starts one that keeps to itself: a daemon of its own,
# and settings of its own under target/apart, so neither your running edualc nor your settings
# are touched. Your Claude Code sessions are shown either way, as they are Claude Code's; but
# messages between sessions reach only the sessions started apart, as Claude Code keeps their
# sockets in the runtime folder too.
def --wrapped "main window" [--release, --apart, ...args: string]: nothing -> nothing {
    cd $root
    let profile = if $release { "release" } else { "debug" }
    let built = if $release { [--release] } else { [] }
    cargo build --locked --quiet ...$built -p edualc -p edualc-server --features edualc-server/server
    let programs = $env.CARGO_TARGET_DIR? | default "target" | path expand | path join $profile
    report-stale ($programs | path join edualcd)
    let window = $programs | path join edualc
    if $apart {
        with-env (apart) { ^$window ...$args }
    } else {
        ^$window ...$args
    }
}

# Follows the daemon's log: the one your windows use, or with --apart the one `try` starts. It
# is in the runtime folder of the protocol this checkout speaks.
def "main log" [--apart]: nothing -> nothing {
    cd $root
    let runtime = if $apart { apart | get XDG_RUNTIME_DIR } else { $env.XDG_RUNTIME_DIR }
    let protocol = open crates/core/src/ipc.rs
        | parse --regex 'pub const PROTOCOL: u32 = (?<number>\d+);'
        | get number.0
    let log = $runtime | path join edualc $"protocol-($protocol)" edualcd.log
    if not ($log | path exists) {
        error make { msg: $"no daemon of protocol ($protocol) has written ($log) yet" }
    }
    ^tail --follow=name $log
}

# What `--apart` changes: a runtime folder of its own, so its daemon is its own, and settings of
# its own. The Wayland display and the session bus stay the desktop's, named by their full paths.
def apart []: nothing -> record {
    let runtime = $env.XDG_RUNTIME_DIR | path join $apart_runtime
    mkdir $runtime
    ^chmod 700 $runtime
    let config = $root | path join $apart_config
    mkdir ($config | path join edualc)
    # Fonts as you have them set up.
    let fonts = $env.HOME | path join .config fontconfig
    let linked = $config | path join fontconfig
    if ($fonts | path exists) and not ($linked | path exists) {
        ^ln -s $fonts $linked
    }
    let display = $env.WAYLAND_DISPLAY? | default "wayland-0"
    let display = if ($display | str starts-with "/") {
        $display
    } else {
        $env.XDG_RUNTIME_DIR | path join $display
    }
    let bus = $env.DBUS_SESSION_BUS_ADDRESS? | default $"unix:path=($env.XDG_RUNTIME_DIR)/bus"
    {
        XDG_RUNTIME_DIR: $runtime
        XDG_CONFIG_HOME: $config
        WAYLAND_DISPLAY: $display
        DBUS_SESSION_BUS_ADDRESS: $bus
    }
}

# Says so if an edualcd built before `daemon` was rebuilt still runs: a window of the same
# protocol joins it, so its side of a change isn't in until it ends. It ends a few seconds after
# its last window and session; Settings → Shut down edualc… ends it at once, with its sessions.
def report-stale [daemon: path]: nothing -> nothing {
    let replaced = $"($daemon) \(deleted\)"
    let stale = ps
        | where name == "edualcd"
        | where {|process| (try { ^readlink $"/proc/($process.pid)/exe" } catch { "" }) == $replaced }
        | get pid
    if ($stale | is-not-empty) {
        print --stderr $"An edualcd from before this build still runs \(($stale | str join ', ')\), and this window joins it: the daemon's side of your change isn't in until it ends. It ends a few seconds after its last window and session; Settings → Shut down edualc… ends it now, with its sessions."
    }
}
