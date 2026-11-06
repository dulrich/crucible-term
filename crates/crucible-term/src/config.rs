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
//! ```
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

fn set_detached_timeout(s: &mut Settings, n: f64) -> Result<(), String> {
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n > f64::from(u32::MAX) {
        return Err(format!(
            "detached-timeout {n}: expected a non-negative integer of seconds"
        ));
    }
    s.detached_timeout = n as u64;
    Ok(())
}

fn set_linger(s: &mut Settings, n: f64) -> Result<(), String> {
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n > f64::from(u32::MAX) {
        return Err(format!(
            "daemon-linger {n}: expected a non-negative integer of seconds"
        ));
    }
    s.daemon_linger = n as u64;
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
    let nonempty = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let base = nonempty("XDG_CONFIG_HOME")
        .filter(|p| p.is_absolute())
        .or_else(|| nonempty("HOME").map(|h| h.join(".config")))?;
    Some(base.join("crucible-term").join("config.kdl"))
}

/// Read and parse the config file through `read`, then apply `overrides` (raw `key=value`
/// strings). Returns the settings and the warning lines for stderr.
pub fn load(
    path: Option<&Path>,
    read: &dyn Fn(&Path) -> std::io::Result<String>,
    overrides: &[String],
) -> (Settings, Vec<String>) {
    let mut s = Settings::default();
    let mut warns = Vec::new();
    if let Some(path) = path {
        match read(path) {
            Ok(src) => parse(&path.display().to_string(), &src, &mut s, &mut warns),
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

/// Parse `src` (named `file` in messages) onto `s`, pushing warning lines.
pub fn parse(file: &str, src: &str, s: &mut Settings, warns: &mut Vec<String>) {
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
    let mut w = |node: &KdlNode, msg: String| {
        let line = line_of(src, node.span().offset());
        warns.push(format!("crucible-term: config: {file}:{line}: {msg}"));
    };
    for node in doc.nodes() {
        let pos = |i: usize| node.get(i);
        match node.name().value() {
            "font" => {
                match pos(0).and_then(KdlValue::as_string) {
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
            "colors" => {
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
            "scrollback" => match node.get("lines").or_else(|| pos(0)).and_then(num) {
                Some(n) => set_scrollback(s, n).unwrap_or_else(|m| w(node, m)),
                None => w(node, "scrollback: expected lines=<n>".into()),
            },
            "pad" => match (
                pos(0).and_then(KdlValue::as_integer),
                pos(1).and_then(KdlValue::as_integer),
            ) {
                (Some(x), Some(y)) => set_pad(s, x, y).unwrap_or_else(|m| w(node, m)),
                _ => w(node, "pad: expected two integers <x> <y>".into()),
            },
            "center" => s.center = true,
            "mode" => match pos(0).and_then(KdlValue::as_string) {
                Some(m) => set_mode(s, m).unwrap_or_else(|m| w(node, m)),
                None => w(node, "mode: expected standalone|daemon|auto".into()),
            },
            "daemon-linger" => match pos(0).and_then(num) {
                Some(n) => set_linger(s, n).unwrap_or_else(|m| w(node, m)),
                None => w(node, "daemon-linger: expected seconds".into()),
            },
            "detached-timeout" => match pos(0).and_then(num) {
                Some(n) => set_detached_timeout(s, n).unwrap_or_else(|m| w(node, m)),
                None => w(node, "detached-timeout: expected seconds".into()),
            },
            "cursor" => {
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
            k => w(node, format!("unknown key {k} (ignored)")),
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

fn apply_key(s: &mut Settings, key: &str, val: &str) -> Result<(), String> {
    let float = || {
        val.parse::<f64>()
            .map_err(|_| format!("{key}: expected a number"))
    };
    match key {
        "font" => {
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
        "font.size" => set_size(s, float()?),
        "colors.foreground" | "colors.background" => {
            set_color(s, key.trim_start_matches("colors."), val)
        }
        "colors.cursor" => {
            // foot's `cursor=<text> <cursor>`: the second colour is the cursor's.
            let c = val.split_whitespace().last().unwrap_or("");
            set_color(s, "cursor", c)
        }
        "colors.alpha" => set_alpha(s, float()?),
        "scrollback" | "scrollback.lines" => set_scrollback(s, float()?),
        "pad" => {
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
        "cursor.shape" | "cursor.style" => set_shape(s, val),
        "mode" => set_mode(s, val),
        "daemon-linger" => set_linger(s, float()?),
        "detached-timeout" => set_detached_timeout(s, float()?),
        "cursor.blink" => {
            s.cursor_blink = parse_bool(val).ok_or("cursor.blink: expected true|false|yes|no")?;
            Ok(())
        }
        k if k.starts_with("colors.palette.") => {
            let i = k["colors.palette.".len()..]
                .parse::<i128>()
                .map_err(|_| "palette: bad index".to_string())?;
            set_palette(s, i, val)
        }
        _ => Err(format!("unknown key {key} (ignored)")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(src: &str) -> (Settings, Vec<String>) {
        let mut s = Settings::default();
        let mut w = Vec::new();
        parse("c.kdl", src, &mut s, &mut w);
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
        let (s, w) = load(Some(Path::new("/c")), &read, &["scrollback=9".into()]);
        assert_eq!((s.scrollback, w.len()), (9, 0));
        let missing = |_: &Path| Err(std::io::ErrorKind::NotFound.into());
        let (s, w) = load(Some(Path::new("/c")), &missing, &[]);
        assert_eq!((s.scrollback, w.len()), (10_000, 0));
        let denied = |_: &Path| Err(std::io::ErrorKind::PermissionDenied.into());
        assert_eq!(load(Some(Path::new("/c")), &denied, &[]).1.len(), 1);
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
}
