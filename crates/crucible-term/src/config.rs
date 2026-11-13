//! KDL configuration: `$XDG_CONFIG_HOME/crucible-term/config.kdl` (fallback
//! `~/.config/crucible-term/config.kdl`), then `-o key=value` overrides on top.
//!
//! ```kdl
//! font "JetBrains Mono" size=11.5          // default: monospace 11
//! colors {
//!     foreground "#dcdccc"                 // "#rrggbb" or "rrggbb"
//!     background "#111111"
//!     cursor "#dcdccc"
//!     alpha 0.9                            // 0.0-1.0; outside is clamped with a warning
//!     palette 1 "#f62b5a"                  // index 0-15
//! }
//! scrollback lines=10000                   // 0-1000000; above is clamped with a warning
//! pad 6 6                                  // logical px, 0-10000 each; default 2 2
//! center                                   // optional: centre the grid in the spare pixels
//! cursor shape="block" blink=#false        // block | underline | beam
//! mode "standalone"                        // standalone | daemon | auto (default standalone)
//! daemon-linger 0                          // daemon idle-exit seconds; 0 = stay resident
//! detached-timeout 30                      // seconds a detached session waits for reattach after a reconnect; 0 = never
//! scrollback-persist #false                // daemon: keep a record of each keyed session on disk (survives reboot)
//! persist-lines 2000                       // daemon: history rows per record (clamped to scrollback)
//! persist-days 14                          // daemon: records older than this are deleted at startup; 0 = never
//! include "~/.config/cde/terminal-theme/cterm.kdl"   // read another file in place
//! ```
//!
//! `include "<path>"` takes one string: `~`/`~/` is `$HOME`, other relative paths resolve against
//! the including file's directory. The file is parsed at that point (later keys win), up to depth
//! 4; a missing, unreadable or malformed included file warns and is skipped.
//!
//! **Errors never stop the terminal.** A whole-file syntax error prints one
//! `crucible-term: config: <file>:<line>: <msg>` line and every key keeps its default. A bad
//! value or unknown key prints one such line and only that key keeps its previous value (its
//! default when it comes from the file; the file's value when it comes from `-o`). Unknown keys
//! are warnings. A missing file is silent.
//!
//! Validation: colours must be `#rrggbb` or `rrggbb`; `alpha` outside 0.0-1.0 is clamped and
//! warned (non-numbers are rejected); `font size` must be in (0, 200]; `scrollback` above
//! 1,000,000 is clamped and warned, negative is rejected; `pad` must be two integers in
//! 0-10000; `cursor shape` must be one of the three names. No value is silently rounded.
//!
//! `-o` keys use the dotted names (`font`, `font.size`, `colors.foreground|background|cursor|
//! alpha`, `colors.palette.<0-15>`, `scrollback`, `pad`, `cursor.shape`, `cursor.blink`) plus
//! foot-compatible aliases: `pad=<x>x<y>[ center]`, `font=<Family>[:size=<n>]`, the
//! `colors-dark.` prefix for `colors.`, `scrollback.lines`, `cursor.style`; a two-colour foot
//! `colors.cursor` (`<text> <cursor>`) takes the second colour. Booleans accept
//! `true|false|yes|no`. An unknown or invalid `-o` key warns (`crucible-term: -o <key>: <msg>`)
//! and is ignored.
//!
//! The daemon keys (`daemon-linger`, `detached-timeout`, `scrollback-persist`, `persist-lines`,
//! `persist-days`) are read from the file the daemon loads when it starts. `scrollback-persist`
//! (default off) keeps a durable record of each keyed session (see `daemon::store`); in the file
//! its value is the KDL boolean `#true` (a bare `true` is a syntax error). `persist-lines` is
//! clamped to `scrollback` when used; `persist-days 0` never ages records out.

use crate::render::{Rgb, Theme};
use crucible_term_core::CursorKind;
use kdl::{KdlDocument, KdlNode, KdlValue};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// The resolved settings, applied before the window starts.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Fontconfig family.
    pub family: String,
    /// Font size in points.
    pub size_pt: f32,
    /// Palette, default colours and background alpha.
    pub theme: Theme,
    /// Scrollback rows.
    pub scrollback: usize,
    /// Padding `(x, y)` in logical pixels.
    pub pad: (u32, u32),
    /// Centre the grid in the spare pixels.
    pub center: bool,
    /// Initial cursor shape (the child's DECSCUSR still overrides it).
    pub cursor_shape: CursorKind,
    /// Initial cursor blink.
    pub cursor_blink: bool,
    /// How `cterm` (no subcommand) runs its window: in-process, through the daemon, or `auto`.
    pub mode: RunMode,
    /// Daemon idle-exit timeout in seconds with no windows and no clients; 0 = stay resident.
    pub daemon_linger: u64,
    /// Seconds a session still detached after a Wayland reconnect waits for a reattach before it
    /// is closed (SIGHUP grace); 0 = never.
    pub detached_timeout: u64,
    /// Daemon: keep a durable record of each keyed session under
    /// `$XDG_STATE_HOME/crucible-term/sessions/` and replay it on the next keyed open.
    pub scrollback_persist: bool,
    /// Daemon: history rows saved per record (clamped to `scrollback`).
    pub persist_lines: usize,
    /// Daemon: records older than this many days are deleted at startup; 0 = never.
    pub persist_days: u64,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            family: "monospace".into(),
            size_pt: 11.0,
            theme: Theme::default(),
            scrollback: 10_000,
            pad: (2, 2),
            center: false,
            cursor_shape: CursorKind::Block,
            cursor_blink: false,
            mode: RunMode::Standalone,
            daemon_linger: 0,
            detached_timeout: 30,
            scrollback_persist: false,
            persist_lines: 2000,
            persist_days: 14,
        }
    }
}

/// The `mode` config key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunMode {
    /// One window in this process (phase-1 behaviour, the default).
    Standalone,
    /// A thin client of the per-user daemon.
    Daemon,
    /// The daemon only when the spawn carries `CRUCIBLE_WINDOW_GRANT`, else standalone.
    Auto,
}

fn set_mode(s: &mut Settings, v: &str) -> Result<(), String> {
    s.mode = match v {
        "standalone" => RunMode::Standalone,
        "daemon" => RunMode::Daemon,
        "auto" => RunMode::Auto,
        _ => return Err(format!("mode {v:?}: expected standalone|daemon|auto")),
    };
    Ok(())
}

/// The seconds field `key` (`daemon-linger` | `detached-timeout`) names.
fn seconds_field<'a>(s: &'a mut Settings, key: &str) -> &'a mut u64 {
    if key == "daemon-linger" {
        &mut s.daemon_linger
    } else {
        &mut s.detached_timeout
    }
}

/// Set a whole-seconds key (`daemon-linger` | `detached-timeout`), up to `u32::MAX`.
fn set_seconds(s: &mut Settings, key: &str, n: f64) -> Result<(), String> {
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n > f64::from(u32::MAX) {
        return Err(format!(
            "{key} {n}: expected a non-negative integer of seconds"
        ));
    }
    *seconds_field(s, key) = n as u64;
    Ok(())
}

/// Set a count key (`persist-lines` | `persist-days`): a non-negative integer, at most
/// [`MAX_SCROLLBACK`] lines or 36500 days.
fn set_count(s: &mut Settings, key: &str, n: f64) -> Result<(), String> {
    let max = if key == "persist-lines" {
        MAX_SCROLLBACK as f64
    } else {
        36_500.0
    };
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n > max {
        return Err(format!("{key} {n}: expected an integer in 0-{max}"));
    }
    if key == "persist-lines" {
        s.persist_lines = n as usize;
    } else {
        s.persist_days = n as u64;
    }
    Ok(())
}

/// Largest accepted scrollback.
pub const MAX_SCROLLBACK: usize = 1_000_000;
/// Largest accepted padding per axis.
const MAX_PAD: u32 = 10_000;
/// Largest accepted font size in points.
const MAX_SIZE: f32 = 200.0;

/// The config file path from an environment lookup (injectable, so tests need no real `$HOME`).
pub fn config_path(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    Some(xdg_home(env, "XDG_CONFIG_HOME", ".config")?.join("config.kdl"))
}

/// `$<var>/crucible-term` when `var` is a non-empty absolute path, else
/// `$HOME/<home_rel>/crucible-term`; `None` without a usable `$HOME`.
pub fn xdg_home(
    env: &dyn Fn(&str) -> Option<OsString>,
    var: &str,
    home_rel: &str,
) -> Option<PathBuf> {
    let nonempty = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = nonempty(var)
        .filter(|p| p.is_absolute())
        .or_else(|| nonempty("HOME").map(|h| h.join(home_rel)))?;
    Some(base.join("crucible-term"))
}

/// What a window spawn adds on top of the config file: its overrides, cwd, app id and title.
pub struct Spawn<'a> {
    /// Raw `-o key=value` overrides.
    pub overrides: &'a [String],
    /// The child's working directory.
    pub cwd: PathBuf,
    /// xdg-toplevel app id, when given.
    pub app_id: Option<&'a str>,
    /// Initial title, when given.
    pub title: Option<&'a str>,
}

/// Resolve a window config: `config.kdl` from the path `env` names, `sp.overrides` on top (each
/// warning printed to stderr), then the spawn's cwd, app id and title.
pub fn window_config(
    env: &dyn Fn(&str) -> Option<OsString>,
    sp: Spawn<'_>,
) -> crate::window::Config {
    let path = config_path(env);
    let (settings, warnings) = load(
        path.as_deref(),
        &|p| std::fs::read_to_string(p),
        env,
        sp.overrides,
    );
    for w in warnings {
        eprintln!("{w}");
    }
    let mut cfg = crate::window::Config {
        settings,
        cwd: sp.cwd,
        ..crate::window::Config::default()
    };
    if let Some(id) = sp.app_id {
        cfg.app_id = id.to_string();
    }
    if let Some(t) = sp.title {
        cfg.title = t.to_string();
    }
    cfg
}

/// Read and parse the config file through `read`, then apply `overrides` (raw `key=value`
/// strings). Returns the settings and the warning lines for stderr.
pub fn load(
    path: Option<&Path>,
    read: &dyn Fn(&Path) -> std::io::Result<String>,
    env: &dyn Fn(&str) -> Option<OsString>,
    overrides: &[String],
) -> (Settings, Vec<String>) {
    let mut s = Settings::default();
    let mut warns = Vec::new();
    let io = Io { read, env };
    if let Some(path) = path {
        match read(path) {
            Ok(src) => parse(&io, path, &src, &mut s, &mut warns, 0),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warns.push(format!(
                "crucible-term: config: {}:1: cannot read: {e}",
                path.display()
            )),
        }
    }
    for o in overrides {
        apply_override(&mut s, o, &mut warns);
    }
    (s, warns)
}

/// 1-based line of a char offset in `src`.
fn line_of(src: &str, offset: usize) -> usize {
    1 + src.chars().take(offset).filter(|&c| c == '\n').count()
}

/// The injectable file reader and environment lookup the parser uses for `include`.
struct Io<'a> {
    read: &'a dyn Fn(&Path) -> std::io::Result<String>,
    env: &'a dyn Fn(&str) -> Option<OsString>,
}

/// Deepest `include` nesting followed: the main file is depth 0, so its includes are depth 1.
const MAX_INCLUDE_DEPTH: usize = 4;

/// Resolve an `include` path: a leading `~` or `~/` is `$HOME`, an absolute path stays, any
/// other path is relative to `base` (the including file's directory). `None` for `~` with no
/// usable `$HOME`.
fn resolve_include(io: &Io<'_>, base: &Path, raw: &str) -> Option<PathBuf> {
    if raw == "~" || raw.starts_with("~/") {
        let home = (io.env)("HOME").filter(|h| !h.is_empty())?;
        return Some(PathBuf::from(home).join(raw[1..].trim_start_matches('/')));
    }
    Some(base.join(raw))
}

/// Apply an `include "<path>"` node: parse the file in place (later keys win), warning and
/// continuing when it is missing, unreadable, too deeply nested or the node is malformed.
fn include(
    io: &Io<'_>,
    file: &Path,
    node: &KdlNode,
    s: &mut Settings,
    depth: usize,
    warns: &mut Vec<String>,
    mut w: impl FnMut(&mut Vec<String>, String),
) {
    let args: Vec<_> = node.entries().iter().collect();
    let raw = match args.as_slice() {
        [e] if e.name().is_none() => e.value().as_string(),
        _ => None,
    };
    let Some(raw) = raw.filter(|r| !r.is_empty()) else {
        w(warns, "include: expected one path string".into());
        return;
    };
    if depth >= MAX_INCLUDE_DEPTH {
        w(
            warns,
            format!("include {raw}: nested deeper than {MAX_INCLUDE_DEPTH} (skipped)"),
        );
        return;
    }
    let base = file.parent().unwrap_or_else(|| Path::new(""));
    let Some(path) = resolve_include(io, base, raw) else {
        w(warns, format!("include {raw}: $HOME is not set (skipped)"));
        return;
    };
    match (io.read)(&path) {
        Ok(src) => parse(io, &path, &src, s, warns, depth + 1),
        Err(e) => w(
            warns,
            format!("include {}: cannot read: {e} (skipped)", path.display()),
        ),
    }
}

/// Parse `src` (the file `path`, named in messages) onto `s`, pushing warning lines. `depth` is
/// the include nesting of this file.
fn parse(
    io: &Io<'_>,
    path: &Path,
    src: &str,
    s: &mut Settings,
    warns: &mut Vec<String>,
    depth: usize,
) {
    let file = path.display().to_string();
    let file = file.as_str();
    let doc = match KdlDocument::parse(src) {
        Ok(d) => d,
        Err(e) => {
            let (line, msg) = e.diagnostics.first().map_or_else(
                || (1, "syntax error".to_string()),
                |d| {
                    (
                        line_of(src, d.span.offset()),
                        d.message.clone().unwrap_or_else(|| "syntax error".into()),
                    )
                },
            );
            warns.push(format!(
                "crucible-term: config: {file}:{line}: {msg} (using defaults)"
            ));
            return;
        }
    };
    for node in doc.nodes() {
        let k = node.name().value();
        let line = line_of(src, node.span().offset());
        if k == "include" {
            include(io, path, node, s, depth, warns, |warns, msg| {
                warns.push(format!("crucible-term: config: {file}:{line}: {msg}"));
            });
            continue;
        }
        let mut w = |node: &KdlNode, msg: String| {
            let line = line_of(src, node.span().offset());
            warns.push(format!("crucible-term: config: {file}:{line}: {msg}"));
        };
        match KEYS
            .iter()
            .find(|d| d.kdl == Some(k))
            .and_then(|d| d.from_kdl)
        {
            Some(set) => set(node, s, &mut w),
            None => w(node, format!("unknown key {k} (ignored)")),
        }
    }
}

fn num(v: &KdlValue) -> Option<f64> {
    v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
}

fn set_size(s: &mut Settings, n: f64) -> Result<(), String> {
    if n.is_finite() && n > 0.0 && n <= f64::from(MAX_SIZE) {
        s.size_pt = n as f32;
        Ok(())
    } else {
        Err(format!(
            "font size {n} out of range (0, 200], keeping default"
        ))
    }
}

fn set_alpha(s: &mut Settings, a: f64) -> Result<(), String> {
    if a.is_nan() {
        return Err("alpha: not a number".into());
    }
    s.theme.alpha = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
    if (0.0..=1.0).contains(&a) {
        Ok(())
    } else {
        Err(format!("alpha {a} outside 0.0-1.0, clamped"))
    }
}

/// Parse `#rrggbb` or `rrggbb`.
pub fn parse_color(v: &str) -> Option<Rgb> {
    let h = v.strip_prefix('#').unwrap_or(v);
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let n = u32::from_str_radix(h, 16).ok()?;
    Some(Rgb((n >> 16) as u8, (n >> 8) as u8, n as u8))
}

fn set_color(s: &mut Settings, key: &str, v: &str) -> Result<(), String> {
    let c = parse_color(v.trim()).ok_or_else(|| format!("{key}: invalid colour {v:?}"))?;
    match key {
        "foreground" => s.theme.fg = c,
        "background" => s.theme.bg = c,
        _ => s.theme.cursor = c,
    }
    Ok(())
}

fn set_palette(s: &mut Settings, i: i128, v: &str) -> Result<(), String> {
    let idx = usize::try_from(i)
        .ok()
        .filter(|&i| i < 16)
        .ok_or_else(|| format!("palette index {i} outside 0-15"))?;
    s.theme.palette[idx] =
        parse_color(v.trim()).ok_or_else(|| format!("palette {idx}: invalid colour {v:?}"))?;
    Ok(())
}

fn set_scrollback(s: &mut Settings, n: f64) -> Result<(), String> {
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 {
        return Err(format!("scrollback {n}: expected a non-negative integer"));
    }
    if n > MAX_SCROLLBACK as f64 {
        s.scrollback = MAX_SCROLLBACK;
        return Err(format!("scrollback {n} above {MAX_SCROLLBACK}, clamped"));
    }
    s.scrollback = n as usize;
    Ok(())
}

fn set_pad(s: &mut Settings, x: i128, y: i128) -> Result<(), String> {
    let ok = |v: i128| u32::try_from(v).ok().filter(|&v| v <= MAX_PAD);
    match (ok(x), ok(y)) {
        (Some(x), Some(y)) => {
            s.pad = (x, y);
            Ok(())
        }
        _ => Err(format!("pad {x} {y}: expected integers in 0-{MAX_PAD}")),
    }
}

fn set_shape(s: &mut Settings, n: &str) -> Result<(), String> {
    s.cursor_shape = match n {
        "block" => CursorKind::Block,
        "underline" => CursorKind::Underline,
        "beam" => CursorKind::Beam,
        _ => return Err(format!("cursor shape {n:?}: expected block|underline|beam")),
    };
    Ok(())
}

fn parse_bool(v: &str) -> Option<bool> {
    match v {
        "true" | "yes" => Some(true),
        "false" | "no" => Some(false),
        _ => None,
    }
}

/// Apply one `-o key=value` override (see the module doc for the key table).
pub fn apply_override(s: &mut Settings, raw: &str, warns: &mut Vec<String>) {
    let Some((key, val)) = raw.split_once('=') else {
        warns.push(format!(
            "crucible-term: -o {raw}: expected key=value (ignored)"
        ));
        return;
    };
    let key = key.trim();
    let key = key
        .strip_prefix("colors-dark.")
        .map_or_else(|| key.to_string(), |rest| format!("colors.{rest}"));
    let r = apply_key(s, &key, val.trim());
    if let Err(m) = r {
        warns.push(format!("crucible-term: -o {raw}: {m}"));
    }
}

/// A warning sink for one KDL node: `(node, message)`.
type Warn<'a> = dyn FnMut(&KdlNode, String) + 'a;
/// Apply a typed KDL node onto the settings, warning per bad value.
type FromKdl = fn(&KdlNode, &mut Settings, &mut Warn<'_>);
/// Apply one `-o` string value; the arguments are the (alias-normalised) key, then the value.
type FromOverride = fn(&mut Settings, &str, &str) -> Result<(), String>;

/// One config key: its KDL node name and its `-o` names, each with its own value parser.
struct Key {
    /// The top-level KDL node name, if the key has one.
    kdl: Option<&'static str>,
    /// The `-o` key names; a name ending in `.` matches as a prefix.
    o: &'static [&'static str],
    /// Typed KDL values onto the settings.
    from_kdl: Option<FromKdl>,
    /// A `-o` string onto the settings.
    from_o: Option<FromOverride>,
}

/// The one key table both the KDL file and `-o` overrides go through.
const KEYS: &[Key] = &[
    Key {
        kdl: Some("font"),
        o: &["font"],
        from_kdl: Some(kdl_font),
        from_o: Some(o_font),
    },
    Key {
        kdl: None,
        o: &["font.size"],
        from_kdl: None,
        from_o: Some(|s, k, v| set_size(s, o_float(k, v)?)),
    },
    Key {
        kdl: Some("colors"),
        o: &["colors.foreground", "colors.background"],
        from_kdl: Some(kdl_colors),
        from_o: Some(|s, k, v| set_color(s, k.trim_start_matches("colors."), v)),
    },
    Key {
        kdl: None,
        o: &["colors.cursor"],
        from_kdl: None,
        // foot's `cursor=<text> <cursor>`: the second colour is the cursor's.
        from_o: Some(|s, _, v| set_color(s, "cursor", v.split_whitespace().last().unwrap_or(""))),
    },
    Key {
        kdl: None,
        o: &["colors.alpha"],
        from_kdl: None,
        from_o: Some(|s, k, v| set_alpha(s, o_float(k, v)?)),
    },
    Key {
        kdl: None,
        o: &["colors.palette."],
        from_kdl: None,
        from_o: Some(|s, k, v| {
            let i = k["colors.palette.".len()..]
                .parse::<i128>()
                .map_err(|_| "palette: bad index".to_string())?;
            set_palette(s, i, v)
        }),
    },
    Key {
        kdl: Some("scrollback"),
        o: &["scrollback", "scrollback.lines"],
        from_kdl: Some(|node, s, w| {
            match node.get("lines").or_else(|| node.get(0)).and_then(num) {
                Some(n) => set_scrollback(s, n).unwrap_or_else(|m| w(node, m)),
                None => w(node, "scrollback: expected lines=<n>".into()),
            }
        }),
        from_o: Some(|s, k, v| set_scrollback(s, o_float(k, v)?)),
    },
    Key {
        kdl: Some("pad"),
        o: &["pad"],
        from_kdl: Some(|node, s, w| {
            match (
                node.get(0).and_then(KdlValue::as_integer),
                node.get(1).and_then(KdlValue::as_integer),
            ) {
                (Some(x), Some(y)) => set_pad(s, x, y).unwrap_or_else(|m| w(node, m)),
                _ => w(node, "pad: expected two integers <x> <y>".into()),
            }
        }),
        from_o: Some(o_pad),
    },
    Key {
        kdl: Some("center"),
        o: &[],
        from_kdl: Some(|_, s, _| s.center = true),
        from_o: None,
    },
    Key {
        kdl: Some("mode"),
        o: &["mode"],
        from_kdl: Some(
            |node, s, w| match node.get(0).and_then(KdlValue::as_string) {
                Some(m) => set_mode(s, m).unwrap_or_else(|m| w(node, m)),
                None => w(node, "mode: expected standalone|daemon|auto".into()),
            },
        ),
        from_o: Some(|s, _, v| set_mode(s, v)),
    },
    Key {
        kdl: Some("daemon-linger"),
        o: &["daemon-linger"],
        from_kdl: Some(kdl_seconds),
        from_o: Some(|s, k, v| set_seconds(s, k, o_float(k, v)?)),
    },
    Key {
        kdl: Some("detached-timeout"),
        o: &["detached-timeout"],
        from_kdl: Some(kdl_seconds),
        from_o: Some(|s, k, v| set_seconds(s, k, o_float(k, v)?)),
    },
    Key {
        kdl: Some("scrollback-persist"),
        o: &["scrollback-persist"],
        from_kdl: Some(kdl_persist),
        from_o: Some(|s, _, v| {
            s.scrollback_persist =
                parse_bool(v).ok_or("scrollback-persist: expected true|false|yes|no")?;
            Ok(())
        }),
    },
    Key {
        kdl: Some("persist-lines"),
        o: &["persist-lines"],
        from_kdl: Some(kdl_count),
        from_o: Some(|s, k, v| set_count(s, k, o_float(k, v)?)),
    },
    Key {
        kdl: Some("persist-days"),
        o: &["persist-days"],
        from_kdl: Some(kdl_count),
        from_o: Some(|s, k, v| set_count(s, k, o_float(k, v)?)),
    },
    Key {
        kdl: Some("cursor"),
        o: &["cursor.shape", "cursor.style"],
        from_kdl: Some(kdl_cursor),
        from_o: Some(|s, _, v| set_shape(s, v)),
    },
    Key {
        kdl: None,
        o: &["cursor.blink"],
        from_kdl: None,
        from_o: Some(|s, _, v| {
            s.cursor_blink = parse_bool(v).ok_or("cursor.blink: expected true|false|yes|no")?;
            Ok(())
        }),
    },
];

fn kdl_font(node: &KdlNode, s: &mut Settings, w: &mut Warn<'_>) {
    match node.get(0).and_then(KdlValue::as_string) {
        Some(f) if !f.is_empty() => s.family = f.to_string(),
        _ => w(node, "font: expected a family name string".into()),
    }
    if let Some(v) = node.get("size") {
        match v.as_float().or_else(|| v.as_integer().map(|i| i as f64)) {
            Some(n) => set_size(s, n).unwrap_or_else(|m| w(node, m)),
            None => w(node, "font size: expected a number".into()),
        }
    }
}

fn kdl_colors(node: &KdlNode, s: &mut Settings, w: &mut Warn<'_>) {
    for c in node.children().map(KdlDocument::nodes).unwrap_or_default() {
        let cv = |i: usize| c.get(i);
        let r = match c.name().value() {
            "alpha" => match cv(0).and_then(num) {
                Some(a) => set_alpha(s, a),
                None => Err("alpha: expected a number".into()),
            },
            "palette" => match (
                cv(0).and_then(KdlValue::as_integer),
                cv(1).and_then(KdlValue::as_string),
            ) {
                (Some(i), Some(col)) => set_palette(s, i, col),
                _ => Err("palette: expected <0-15> \"#rrggbb\"".into()),
            },
            k @ ("foreground" | "background" | "cursor") => {
                match cv(0).and_then(KdlValue::as_string) {
                    Some(col) => set_color(s, k, col),
                    None => Err(format!("{k}: expected \"#rrggbb\"")),
                }
            }
            k => Err(format!("unknown key colors.{k} (ignored)")),
        };
        if let Err(m) = r {
            w(c, m);
        }
    }
}

fn kdl_seconds(node: &KdlNode, s: &mut Settings, w: &mut Warn<'_>) {
    let key = node.name().value();
    match node.get(0).and_then(num) {
        Some(n) => set_seconds(s, key, n).unwrap_or_else(|m| w(node, m)),
        None => w(node, format!("{key}: expected seconds")),
    }
}

/// `scrollback-persist` alone, or with `#true` / `#false` (a `"true"`-style string is accepted too).
fn kdl_persist(node: &KdlNode, s: &mut Settings, w: &mut Warn<'_>) {
    let v = match node.get(0) {
        None => Some(true),
        Some(v) => v.as_bool().or_else(|| v.as_string().and_then(parse_bool)),
    };
    match v {
        Some(b) => s.scrollback_persist = b,
        None => w(node, "scrollback-persist: expected #true or #false".into()),
    }
}

fn kdl_count(node: &KdlNode, s: &mut Settings, w: &mut Warn<'_>) {
    let key = node.name().value();
    match node.get(0).and_then(num) {
        Some(n) => set_count(s, key, n).unwrap_or_else(|m| w(node, m)),
        None => w(node, format!("{key}: expected a number")),
    }
}

fn kdl_cursor(node: &KdlNode, s: &mut Settings, w: &mut Warn<'_>) {
    if let Some(v) = node.get("shape") {
        match v.as_string() {
            Some(n) => set_shape(s, n).unwrap_or_else(|m| w(node, m)),
            None => w(node, "cursor shape: expected a string".into()),
        }
    }
    if let Some(v) = node.get("blink") {
        match v.as_bool() {
            Some(b) => s.cursor_blink = b,
            None => w(node, "cursor blink: expected #true or #false".into()),
        }
    }
}

/// A `-o` number value; the error names the key.
fn o_float(key: &str, val: &str) -> Result<f64, String> {
    val.parse::<f64>()
        .map_err(|_| format!("{key}: expected a number"))
}

fn o_font(s: &mut Settings, _: &str, val: &str) -> Result<(), String> {
    let mut parts = val.split(':');
    let fam = parts.next().unwrap_or("").trim();
    if fam.is_empty() {
        return Err("font: empty family".into());
    }
    let mut size = None;
    for p in parts {
        match p.trim().split_once('=') {
            Some(("size", n)) => {
                size = Some(
                    n.trim()
                        .parse::<f64>()
                        .map_err(|_| format!("font size {n:?}: expected a number"))?,
                )
            }
            _ => return Err(format!("font attribute {p:?} unsupported (only size=)")),
        }
    }
    if let Some(n) = size {
        set_size(s, n)?;
    }
    s.family = fam.to_string();
    Ok(())
}

fn o_pad(s: &mut Settings, _: &str, val: &str) -> Result<(), String> {
    let mut words = val.split_whitespace();
    let dims = words.next().unwrap_or("");
    let center = match words.next() {
        None => false,
        Some("center") if words.next().is_none() => true,
        Some(_) => return Err("pad: expected <x>x<y> [center]".into()),
    };
    let (x, y) = dims
        .split_once('x')
        .and_then(|(x, y)| Some((x.parse::<i128>().ok()?, y.parse::<i128>().ok()?)))
        .ok_or("pad: expected <x>x<y> [center]")?;
    set_pad(s, x, y)?;
    s.center = center;
    Ok(())
}

fn apply_key(s: &mut Settings, key: &str, val: &str) -> Result<(), String> {
    let hit = |n: &&str| match n.strip_suffix('.') {
        Some(_) => key.starts_with(*n),
        None => key == *n,
    };
    match KEYS
        .iter()
        .find(|d| d.o.iter().any(hit))
        .and_then(|d| d.from_o)
    {
        Some(set) => set(s, key, val),
        None => Err(format!("unknown key {key} (ignored)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(src: &str) -> (Settings, Vec<String>) {
        let mut s = Settings::default();
        let mut w = Vec::new();
        let read = |_: &Path| Err(std::io::ErrorKind::NotFound.into());
        let env = |_: &str| None;
        parse(
            &Io {
                read: &read,
                env: &env,
            },
            Path::new("c.kdl"),
            src,
            &mut s,
            &mut w,
            0,
        );
        (s, w)
    }

    fn ov(s: &mut Settings, o: &str) -> Vec<String> {
        let mut w = Vec::new();
        apply_override(s, o, &mut w);
        w
    }

    #[test]
    fn path_follows_xdg_then_home() {
        let env = |m: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                m.iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| OsString::from(v))
            }
        };
        let both = env(&[("XDG_CONFIG_HOME", "/x"), ("HOME", "/h")]);
        assert_eq!(
            config_path(&both),
            Some("/x/crucible-term/config.kdl".into())
        );
        let home = env(&[("HOME", "/h")]);
        assert_eq!(
            config_path(&home),
            Some("/h/.config/crucible-term/config.kdl".into())
        );
        assert_eq!(config_path(&env(&[])), None);
    }

    #[test]
    fn full_file_applies() {
        let (s, w) = parsed(
            "font \"Iosevka\" size=12.5\ncolors {\n  foreground \"#112233\"\n  background \"445566\"\n  alpha 0.5\n  palette 1 \"#ff0000\"\n}\nscrollback lines=500\npad 6 4\ncenter\ncursor shape=\"beam\" blink=#true\n",
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!((s.family.as_str(), s.size_pt), ("Iosevka", 12.5));
        assert_eq!(s.theme.fg, Rgb(0x11, 0x22, 0x33));
        assert_eq!(s.theme.palette[1], Rgb(255, 0, 0));
        assert_eq!(s.theme.alpha, 128);
        assert_eq!((s.scrollback, s.pad, s.center), (500, (6, 4), true));
        assert_eq!((s.cursor_shape, s.cursor_blink), (CursorKind::Beam, true));
    }

    #[test]
    fn bad_values_warn_and_keep_defaults() {
        let (s, w) = parsed(
            "font \"X\" size=0\ncolors {\n  background \"red\"\n  alpha 2\n  palette 16 \"#000000\"\n}\nscrollback lines=2000000\npad -1 2\ncursor shape=\"wiggle\"\nbogus 1\n",
        );
        let d = Settings::default();
        assert_eq!(s.size_pt, d.size_pt);
        assert_eq!(s.theme.bg, d.theme.bg);
        assert_eq!(s.theme.alpha, 255);
        assert_eq!(s.scrollback, MAX_SCROLLBACK);
        assert_eq!((s.pad, s.cursor_shape), (d.pad, d.cursor_shape));
        assert_eq!(s.family, "X");
        assert_eq!(w.len(), 8, "{w:#?}");
        assert!(w[0].starts_with("crucible-term: config: c.kdl:1: font size"));
        assert!(w[1].contains("c.kdl:3:"));
        assert!(w.iter().any(|m| m.contains("c.kdl:10: unknown key bogus")));
    }

    #[test]
    fn syntax_error_gives_defaults_and_one_line() {
        let (s, w) = parsed("font \"ok\"\npad {{{\n");
        assert_eq!(s.family, "monospace");
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("crucible-term: config: c.kdl:"), "{w:?}");
    }

    #[test]
    fn load_reads_through_the_injected_reader() {
        let read = |_: &Path| Ok("scrollback lines=7\n".to_string());
        let (s, w) = load(
            Some(Path::new("/c")),
            &read,
            &|_| None,
            &["scrollback=9".into()],
        );
        assert_eq!((s.scrollback, w.len()), (9, 0));
        let missing = |_: &Path| Err(std::io::ErrorKind::NotFound.into());
        let (s, w) = load(Some(Path::new("/c")), &missing, &|_| None, &[]);
        assert_eq!((s.scrollback, w.len()), (10_000, 0));
        let denied = |_: &Path| Err(std::io::ErrorKind::PermissionDenied.into());
        assert_eq!(
            load(Some(Path::new("/c")), &denied, &|_| None, &[]).1.len(),
            1
        );
    }

    #[test]
    fn cde_pad_and_foot_font_overrides() {
        let mut s = Settings::default();
        assert!(ov(&mut s, "pad=6x6 center").is_empty());
        assert_eq!((s.pad, s.center), ((6, 6), true));
        assert!(ov(&mut s, "pad=3x5").is_empty());
        assert_eq!((s.pad, s.center), ((3, 5), false));
        assert!(ov(&mut s, "font=Fira Code:size=13").is_empty());
        assert_eq!((s.family.as_str(), s.size_pt), ("Fira Code", 13.0));
        assert_eq!(ov(&mut s, "pad=6").len(), 1);
        assert_eq!(ov(&mut s, "pad=-1x2").len(), 1);
        assert_eq!(s.pad, (3, 5));
    }

    #[test]
    fn colour_aliases_and_ignored_keys() {
        let mut s = Settings::default();
        assert!(ov(&mut s, "colors-dark.background=0a0b0c").is_empty());
        assert!(ov(&mut s, "colors-dark.alpha=0.8").is_empty());
        assert_eq!(s.theme.bg, Rgb(10, 11, 12));
        assert_eq!(s.theme.alpha, 204);
        assert!(ov(&mut s, "colors.cursor=111111 222222").is_empty());
        assert_eq!(s.theme.cursor, Rgb(0x22, 0x22, 0x22));
        assert!(ov(&mut s, "cursor.blink=yes").is_empty());
        assert!(s.cursor_blink);
        assert_eq!(ov(&mut s, "nonsense=1").len(), 1);
        assert_eq!(ov(&mut s, "no-equals").len(), 1);
        assert_eq!(ov(&mut s, "colors.alpha=abc").len(), 1);
        assert_eq!(ov(&mut s, "colors.alpha=7").len(), 1);
        assert_eq!(s.theme.alpha, 255);
    }

    #[test]
    fn seconds_keys_share_one_setter_from_both_sources() {
        let (mut s, w) = parsed("daemon-linger 5\ndetached-timeout 1.5\nmode \"auto\"\n");
        assert_eq!((s.daemon_linger, s.detached_timeout), (5, 30));
        assert_eq!(s.mode, RunMode::Auto);
        assert_eq!(
            w,
            [
                "crucible-term: config: c.kdl:2: detached-timeout 1.5: expected a non-negative integer of seconds"
            ]
        );
        assert!(ov(&mut s, "detached-timeout=7").is_empty());
        assert_eq!(
            ov(&mut s, "daemon-linger=-1"),
            [
                "crucible-term: -o daemon-linger=-1: daemon-linger -1: expected a non-negative integer of seconds"
            ]
        );
        assert_eq!((s.daemon_linger, s.detached_timeout), (5, 7));
        assert_eq!(
            ov(&mut s, "colors.palette.x=#000000"),
            ["crucible-term: -o colors.palette.x=#000000: palette: bad index"]
        );
    }
    #[test]
    fn persist_keys_default_off_and_parse_from_both_sources() {
        let d = Settings::default();
        assert_eq!(
            (d.scrollback_persist, d.persist_lines, d.persist_days),
            (false, 2000, 14)
        );
        let (mut s, w) = parsed("scrollback-persist #true\npersist-lines 500\npersist-days 0\n");
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(
            (s.scrollback_persist, s.persist_lines, s.persist_days),
            (true, 500, 0)
        );
        assert!(parsed("scrollback-persist\n").0.scrollback_persist);
        // KDL v2: a bare `true` is a syntax error (the whole file falls back to defaults).
        assert!(!parsed("scrollback-persist true\n").0.scrollback_persist);
        assert!(!parsed("scrollback-persist #false\n").0.scrollback_persist);
        assert_eq!(parsed("persist-lines -1\n").1.len(), 1);
        assert!(ov(&mut s, "scrollback-persist=no").is_empty());
        assert!(ov(&mut s, "persist-days=30").is_empty());
        assert_eq!((s.scrollback_persist, s.persist_days), (false, 30));
        assert_eq!(ov(&mut s, "persist-days=1.5").len(), 1);
    }

    /// Load `/h/c.kdl` from an in-memory file map with `HOME=/hm/u`.
    fn load_files(files: &[(&str, &str)]) -> (Settings, Vec<String>) {
        let map: std::collections::HashMap<PathBuf, String> = files
            .iter()
            .map(|(p, c)| (PathBuf::from(p), (*c).to_string()))
            .collect();
        let read = move |p: &Path| {
            map.get(p)
                .cloned()
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))
        };
        let env = |k: &str| (k == "HOME").then(|| OsString::from("/hm/u"));
        load(Some(Path::new("/h/c.kdl")), &read, &env, &[])
    }

    #[test]
    fn include_expands_tilde_and_resolves_relative_to_the_including_dir() {
        let (s, w) = load_files(&[
            ("/h/c.kdl", "include \"~/t/a.kdl\"\ninclude \"sub/b.kdl\"\n"),
            ("/hm/u/t/a.kdl", "scrollback lines=11\n"),
            ("/h/sub/b.kdl", "pad 3 4\n"),
        ]);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!((s.scrollback, s.pad), (11, (3, 4)));
    }

    #[test]
    fn include_later_keys_win_in_both_directions() {
        let inc = ("/h/i.kdl", "scrollback lines=5\n");
        let (s, _) = load_files(&[("/h/c.kdl", "scrollback lines=1\ninclude \"i.kdl\"\n"), inc]);
        assert_eq!(s.scrollback, 5, "included file overrides earlier keys");
        let (s, _) = load_files(&[("/h/c.kdl", "include \"i.kdl\"\nscrollback lines=1\n"), inc]);
        assert_eq!(s.scrollback, 1, "later keys override the included file");
    }

    #[test]
    fn include_missing_file_warns_once_and_continues() {
        let (s, w) = load_files(&[("/h/c.kdl", "include \"gone.kdl\"\nscrollback lines=8\n")]);
        assert_eq!(s.scrollback, 8);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].contains("c.kdl:1") && w[0].contains("/h/gone.kdl"),
            "{w:?}"
        );
    }

    #[test]
    fn include_nests_and_stops_past_depth_four() {
        let files = [
            ("/h/c.kdl", "include \"1.kdl\"\n"),
            ("/h/1.kdl", "include \"2.kdl\"\n"),
            ("/h/2.kdl", "include \"3.kdl\"\n"),
            ("/h/3.kdl", "include \"4.kdl\"\n"),
            ("/h/4.kdl", "scrollback lines=4\ninclude \"5.kdl\"\n"),
            ("/h/5.kdl", "scrollback lines=5\n"),
        ];
        let (s, w) = load_files(&files);
        assert_eq!(s.scrollback, 4, "depth 4 is read, depth 5 is not");
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].contains("4.kdl:2") && w[0].contains("nested deeper"),
            "{w:?}"
        );
    }

    #[test]
    fn include_cycle_is_bounded() {
        let (_, w) = load_files(&[("/h/c.kdl", "include \"c.kdl\"\n")]);
        assert_eq!(w.len(), 1, "{w:?}");
    }

    #[test]
    fn include_malformed_shapes_and_broken_included_file_warn() {
        let (s, w) = load_files(&[
            (
                "/h/c.kdl",
                "include\ninclude 5\ninclude \"a\" \"b\"\ninclude \"bad.kdl\"\nscrollback lines=9\n",
            ),
            ("/h/bad.kdl", "pad {{{\n"),
        ]);
        assert_eq!(s.scrollback, 9);
        assert_eq!(w.len(), 4, "{w:?}");
    }
}
