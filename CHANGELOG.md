# Changelog

All notable changes to crucible-term are recorded here, in the [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) style.

crucible-term uses pre-1.0 semantic versioning: a breaking change to the config or the command-line contract CDE spawns through bumps the minor version; anything else bumps the patch.

## 0.1.0 — 2026-11-05

### Added

- Per-window terminal on the rio-vt 0.5.29 VT core (plan 01, phase 1): one process per window with its own PTY, behind the `crucible-term-core` session seam (snapshot, cell-level delta, ED 1 correction).
- CPU renderer and Wayland window: fontconfig font discovery, glyph cache, fractional scale, background alpha.
- Input: xkbcommon keymaps and repeat, legacy and kitty keyboard encoding, mouse reporting (1000/1002/1003/1006) with Shift overriding it for local selection, bracketed paste and focus events.
- Selection (word, line, block) into the primary selection with middle-click paste; clipboard copy/paste on `Ctrl+Shift+C`/`Ctrl+Shift+V`; OSC 52 write while focused; scrollback viewport on `Shift+PageUp/PageDown`, `Shift+Home/End` and the wheel.
- KDL config at `$XDG_CONFIG_HOME/crucible-term/config.kdl` (font, colours and palette, alpha, scrollback, padding, cursor); a broken file falls back to defaults.
- CLI contract for CDE: `-e`, `--working-directory`, `-o key=value` with foot-compatible aliases (`pad=<x>x<y> center`, `font=<family>:size=<n>`, `colors-dark.*`), `--app-id`, `--title`; the shell's exit status is the process exit status; child exit detected through a pidfd.
- `scripts/deploy.sh <host-a|host-b|all>` and the G1 measurement harness `scripts/g1/` with its results in `docs/g1-measurement.md`.

### Known gaps

- G1 measured part: PSS is 60% above foot (limit 25%); 50 MB throughput is at parity; the unicode corpus is 1.47x slower than foot. User acceptance is outstanding, and foot stays CDE's default terminal until G1 is decided.
- rio-vt records only OSC 133 A/P; OSC 7 with a non-local host is dropped; OSC 10/11 and OSC 52 reads are not answered; `CSI 0 q` resets to a steady block rather than the configured cursor; underline colour (SGR 58) is not drawn; some kitty keyboard flags are encoded as unset.
