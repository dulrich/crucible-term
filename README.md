# crucible-term

The terminal for Crucible Dev Env (CDE): a Rust Wayland terminal built on the rio-vt core, with a fast CPU renderer and light resource use. It is designed to grow into a per-user daemon whose sessions survive window loss and can be attached to from other hosts.

The executable is `cterm` (the crate and project are `crucible-term`). Status: plan 01 (the per-window terminal) is implemented as 0.1.0; the executable was renamed to `cterm` in 0.2.0. The roadmap is in [`plans/00-roadmap.md`](plans/00-roadmap.md).

## Usage

```
cterm [--working-directory DIR] [-o KEY=VALUE]... [--app-id ID] [--title T] [-e CMD [ARG]...]
```

- `-e CMD [ARG]...` runs CMD; everything after `-e` is the child's argv, verbatim. Without it the window runs `$SHELL` (fallback `/bin/sh`).
- `--working-directory DIR` (or `--working-directory=DIR`) starts the child in DIR; a missing DIR warns and falls back to the current directory.
- `-o KEY=VALUE` / `--override KEY=VALUE` (repeatable) overrides a config key, applied after the config file. Unknown or invalid keys warn and are ignored.
- `--app-id ID` sets the xdg app id (default `crucible-term`); `--title T` sets the initial title (OSC 0/2 still updates it).
- `--help`, `--build`, `--version` answer without reading any config. An unknown flag prints usage on stderr and exits 2.
- The process exit status is the child's exit code (`128 + signal` when killed). The window closes when the child exits, even if a background process still holds the terminal.

## Config

`$XDG_CONFIG_HOME/crucible-term/config.kdl` (fallback `~/.config/crucible-term/config.kdl`), KDL. Every key is optional.

```kdl
font "JetBrains Mono" size=11.5      // default: monospace 11; size in (0, 200]
colors {
    foreground "#dcdccc"             // "#rrggbb" or "rrggbb" (quoted strings)
    background "#111111"
    cursor "#dcdccc"
    alpha 0.9                        // 0.0-1.0, clamped with a warning
    palette 1 "#f62b5a"              // index 0-15
}
scrollback lines=10000               // default 10000, at most 1000000 (clamped with a warning)
pad 6 6                              // logical px, default 2 2
center                               // optionally centre the grid in the spare pixels
cursor shape="block" blink=#false    // block | underline | beam; the child's DECSCUSR still overrides
```

A broken file never stops the terminal: each problem prints one `crucible-term: config: <file>:<line>: <msg>` line on stderr and that key keeps its default; a whole-file syntax error keeps all defaults. Unknown keys warn and are ignored.

### `-o` keys

| Key | Meaning |
| --- | --- |
| `font=<family>` / `font=<Family>:size=<n>` | family (foot syntax with size) |
| `font.size=<pt>` | font size |
| `colors.foreground`, `colors.background`, `colors.cursor` = `rrggbb` | colours |
| `colors.alpha=<0.0-1.0>` | background alpha |
| `colors.palette.<0-15>=rrggbb` | palette entry |
| `colors-dark.<key>=...` | foot alias for `colors.<key>` (CDE's overlay args use `colors-dark.background` and `colors-dark.alpha`) |
| `scrollback=<n>` (alias `scrollback.lines`) | scrollback rows |
| `pad=<x>x<y>` or `pad=<x>x<y> center` | padding (CDE's `pad={gap}x{gap} center`, as one argument) |
| `cursor.shape=block\|underline\|beam` (alias `cursor.style`), `cursor.blink=true\|false\|yes\|no` | cursor |
| `mode=standalone\|daemon\|auto` | how the window runs (see Modes) |
| `daemon-linger=<s>` | daemon idle-exit seconds (see Modes) |
| `detached-timeout=<s>` | seconds a detached session waits for reattach after a reconnect; 0 = never (see Modes) |

## Modes

- Config key `mode` (file `mode "auto"` or `-o mode=…`): `standalone` (default, phase-1 behaviour: the window runs in this process), `daemon` (this `cterm` is a thin client of the per-user daemon, which owns the PTY and the window), or `auto` (the daemon only when the spawn carries `CRUCIBLE_WINDOW_GRANT`, else standalone). `--standalone` forces standalone whatever the config says.
- `auto` stays standalone in practice until CDE grants window identity: the daemon answers a granted `open` with `no-identity`, and the client falls back.
- Socket: `$XDG_RUNTIME_DIR/cterm/<basename of $WAYLAND_DISPLAY>.sock` in a `0700` directory; only the daemon's own UID is served. Lock: the sibling `.lock`, taken by a client that starts the daemon. Log: `$XDG_STATE_HOME/crucible-term/daemon.log` (fallback `~/.local/state/crucible-term/daemon.log`), appended.
- The daemon starts on demand (`cterm daemon`, detached) and stays resident after its last window. `daemon-linger N` (seconds, in the config the daemon reads when it starts; absent or 0 = off) makes it exit after N s with no windows and no clients.
- Wayland loss: the daemon *detaches* instead of exiting. Every session (PTY, child, VT state) stays live and keeps draining its PTY; a client EOF no longer closes a detached session; a child that exits while detached is still relayed. The daemon retries the display it started on every second. An `open` while detached tries the display once and is refused `no-wayland` if it is still gone (the client falls back to standalone). Standalone mode still exits on Wayland loss.
- Reattach by session key: CDE exports `CRUCIBLE_TERM_SESSION` (32 lowercase hex) to each granted spawn; the client sends it in `open` (a malformed value is ignored with one warning) and strips it from the child's environment on every path, like the grant. A *granted* open whose key names a *detached* session rebinds a new window to that live session (same root pid) instead of spawning; the previous client, if still connected, gets `superseded` and exits 0 silently. A key that names an attached session is never stolen (the new window spawns, unregistered, and the daemon logs it), and a key without a grant never reattaches.
- `detached-timeout N` (seconds, default 30, 0 = never; daemon config): when the display comes back, each session still detached gets N s to be reattached, else it is closed through the SIGHUP grace and logged. A reattach cancels its clock; another loss before expiry cancels every clock until the next reconnect.
- Fallback: no `XDG_RUNTIME_DIR` or `WAYLAND_DISPLAY`, a daemon that cannot be reached or started within 2 s, a protocol-version mismatch (the wire is v2), or a `no-identity` or `no-wayland` refusal → the client runs the window standalone in-process with one warning.
- The client exits with its window's exit code. SIGTERM/SIGINT/SIGHUP to the client, or its death, closes the window (the child gets SIGHUP with a 500 ms grace). If the daemon dies the client exits 1 (`crucible-term: daemon connection lost`); `kill -9` of the daemon closes every window.
- The child environment never contains `CRUCIBLE_WINDOW_GRANT`, in any mode.

## Using it from CDE

CDE spawns the terminal directly (no shell) as `[terminal, <terminal-args…>, <exec-flag>, <cmd…>]` and sets the working directory itself. In your CDE config:

```kdl
terminal "/path/to/cterm" exec-flag="-e"
```

with the existing `terminal-args "-o" "pad={gap}x{gap} center"` unchanged. Switching CDE to cterm is the user's decision.

## Install

Build with `cargo build --release`, then install the binary as `cterm` (for example `~/.local/bin/cterm`) and the launcher entry `share/cterm.desktop` into `~/.local/share/applications` (CDE's `Super+Return` launcher lists it as a TERM row). A binary left under the old name `crucible-term` can be removed.
