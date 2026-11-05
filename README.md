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

## Using it from CDE

CDE spawns the terminal directly (no shell) as `[terminal, <terminal-args…>, <exec-flag>, <cmd…>]` and sets the working directory itself. In `the private config repo/cde/config-<host>.kdl`:

```kdl
terminal "~/.local/bin/cterm" exec-flag="-e"
```

with the existing `terminal-args "-o" "pad={gap}x{gap} center"` unchanged. Switching CDE to cterm is the user's decision.

## Deploy

`scripts/deploy.sh <host-a|host-b|all>` builds the release binary, installs `~/.local/bin/cterm` (keeping `cterm.prev`, and removing a binary left under the old name `crucible-term`), installs the launcher entry `share/cterm.desktop` into `~/.local/share/applications` (CDE's `Super+Return` launcher lists it as a TERM row), syncs the other host by git bundle (a first deploy clones the repo there), and prints `RESULT: <host> cterm <version> (<build-id>)`. It never pushes and never restarts CDE.
