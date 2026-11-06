# Changelog

All notable changes to crucible-term are recorded here, in the [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) style.

crucible-term uses pre-1.0 semantic versioning: a breaking change to the config or the command-line contract CDE spawns through bumps the minor version; anything else bumps the patch.

## 0.3.0 — 2026-11-05

### Added

- Daemon mode (plan 02, phase 2): `cterm daemon` owns every PTY and VT session on one Wayland connection with shared font caches; `cterm` becomes a thin client that hands argv/cwd/env to the daemon and exits with the shell's status. The daemon is spawned on demand (socket and lock under `$XDG_RUNTIME_DIR/cterm/`, log under `$XDG_STATE_HOME/crucible-term/daemon.log`) and lingers per `daemon-linger`.
- Config key `mode` (`standalone` | `auto`) and `--standalone`. **The default stays `standalone`**; `mode "auto"` uses the daemon and falls back to standalone when it is unavailable or refuses.
- Window identity: in daemon mode each window binds CDE's `crucible-window-identity-v1` grant (minted by CDE at spawn), so CDE places, captures and closes daemon windows by their own shell, not the daemon's pid. Without the global a granted open falls back to standalone.
- Hidden windows free their full-size shm buffers while CDE marks them `suspended` (standalone and daemon), and repaint in full on re-show.
- `scripts/g2/g2.sh`: the G2 measurement and end-to-end reversed-arrival harness.

### Changed

- Rendering paints straight into the shm buffer by buffer age (no persistent frame image); release builds use a fat-LTO profile. Standalone memory is now within 10% of foot.

### G2

- Measured in `docs/g2-measurement.md` § G2 result. Passes: reversed-arrival (unit and end to end), standalone window ≤ foot + 10% (+8.6%), hidden-window marginal ≤ 10 MiB (6.97 MiB), re-show content checksums. **Fails**: a visible daemon window is foot + 11.9% (limit + 5%), and warm-daemon first frame is 81.3 ms against G1's 64.2 ms (though below same-batch standalone's 149.9 ms). Per the plan, `mode` stays `standalone` and the user decides whether phase 3 proceeds.

## 0.2.0 — 2026-11-05

### Changed

- The executable is now `cterm` (the crate and project stay `crucible-term`; the config path, app id and stderr prefix are unchanged). `--help` and `--version` name `cterm`. `scripts/deploy.sh` installs `~/.local/bin/cterm` and removes a binary left under the old name. A CDE config pointing at `crucible-term` must point at `cterm` instead.

### Added

- Launcher entry `share/cterm.desktop` (`Categories=System;TerminalEmulator;`), installed by `scripts/deploy.sh` into `~/.local/share/applications`, so CDE's program launcher (`Super+Return`) lists `cterm` as a TERM row.

### G1

- The user accepted the measured part of G1 despite the memory miss (PSS +60% vs foot, limit +25%); the roadmap continues.

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
