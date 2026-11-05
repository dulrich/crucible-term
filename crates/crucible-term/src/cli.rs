//! The command line CDE spawns the terminal through: a pure parser over the argument vector.
//!
//! ```text
//! cterm [--standalone] [--working-directory DIR] [-o KEY=VALUE]... [--app-id ID] [--title T] [-e CMD [ARG]...]
//! cterm daemon [--help]
//! ```
//!
//! Everything after `-e` is the child's argv, verbatim (CDE's `exec-flag`), so `--help` after
//! `-e` belongs to the child. `--help`/`-h`, `--build` and `--version` answer without reading
//! any config. An unknown flag, a flag missing its value, or `-e` with nothing after it is a
//! [`CliError`]; the caller prints usage on stderr and exits 2.

use std::ffi::OsString;
use std::path::PathBuf;

/// The usage text (`--help` prints it on stdout; errors print it on stderr).
pub const USAGE: &str = "usage: cterm [OPTIONS] [-e CMD [ARG]...]\n\nOpen a terminal window running CMD (default: $SHELL, fallback /bin/sh); the exit status is\nthe child's (128 + signal when killed).\n\n  -e CMD [ARG]...          run CMD with its arguments; everything after -e is the argv\n  --working-directory DIR  start the child in DIR (a missing DIR warns and uses the current dir)\n  -o, --override KEY=VAL   override a config key (repeatable), e.g. -o pad=6x6\\ center\n                           -o font=Mono:size=12 -o colors.alpha=0.9 -o scrollback=5000\n  --app-id ID              xdg app id (default crucible-term)\n  --title TITLE            initial window title (the child can still change it)\n  --standalone             run this window in-process even when config `mode` is daemon/auto\n  --build                  print the tool-scoped build id (40-hex commit, `-dirty` if modified)\n  --version                print `cterm <semver> (<build id>)`\n  -h, --help               print this usage and exit 0\n\nSubcommands:\n  daemon                   run the per-user window daemon (see `cterm daemon --help`)\n\nConfig: $XDG_CONFIG_HOME/crucible-term/config.kdl (fallback ~/.config/crucible-term/config.kdl).\n";

/// The `cterm daemon --help` text.
pub const DAEMON_USAGE: &str = "usage: cterm daemon [-h | --help]\n\nRun the per-user crucible-term daemon in the foreground: it owns the PTYs and windows of every\n`cterm` thin client on this Wayland display. Clients start it on demand (config `mode daemon`,\nor `auto` with a CDE grant); running it by hand is only needed for debugging.\n\nSocket: $XDG_RUNTIME_DIR/cterm/<WAYLAND_DISPLAY basename>.sock (lock: the sibling .lock).\nLog:    $XDG_STATE_HOME/crucible-term/daemon.log when started by a client.\nIt stays resident after its last window; config `daemon-linger N` exits after N idle seconds.\nIt exits when its Wayland connection is lost; killing it closes every window.\n";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    /// Print [`USAGE`] and exit 0.
    Help,
    /// Print the build id and exit 0.
    Build,
    /// Print the version line and exit 0.
    Version,
    /// Open a window.
    Run(Cli),
    /// Run the daemon (`cterm daemon`).
    Daemon,
    /// Print [`DAEMON_USAGE`] and exit 0.
    DaemonHelp,
}

/// The options of a window run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Cli {
    /// The child's argv after `-e`; `None` means the shell.
    pub exec: Option<Vec<OsString>>,
    /// `--working-directory`.
    pub working_directory: Option<PathBuf>,
    /// `-o` values, raw `key=value`, in order.
    pub overrides: Vec<String>,
    /// `--app-id`.
    pub app_id: Option<String>,
    /// `--title`.
    pub title: Option<String>,
    /// `--standalone`: force the in-process mode.
    pub standalone: bool,
}

/// A command-line mistake; the message is one line without the `crucible-term:` prefix.
#[derive(Debug, PartialEq, Eq)]
pub struct CliError(pub String);

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CliError {}

/// Parse the arguments after `argv[0]`.
pub fn parse(args: Vec<OsString>) -> Result<Parsed, CliError> {
    // `daemon` is a subcommand only as the first argument.
    if args.first().is_some_and(|a| a == "daemon") {
        return match args.get(1).and_then(|a| a.to_str()) {
            None => Ok(Parsed::Daemon),
            Some("-h" | "--help") if args.len() == 2 => Ok(Parsed::DaemonHelp),
            Some("--build") if args.len() == 2 => Ok(Parsed::Build),
            Some("--version") if args.len() == 2 => Ok(Parsed::Version),
            _ => Err(CliError(format!(
                "daemon: unexpected argument {:?}",
                args[1]
            ))),
        };
    }
    let mut cli = Cli::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let Some(a) = arg.to_str() else {
            return Err(CliError(format!("argument {arg:?} is not valid UTF-8")));
        };
        // `--flag=value` splits here; flags that take no value reject a `=`.
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (a, None),
        };
        let value = |it: &mut std::vec::IntoIter<OsString>| -> Result<OsString, CliError> {
            if let Some(v) = &inline {
                return Ok(v.into());
            }
            it.next()
                .ok_or_else(|| CliError(format!("{flag} needs a value")))
        };
        let text = |v: OsString, flag: &str| -> Result<String, CliError> {
            v.into_string()
                .map_err(|_| CliError(format!("{flag} value is not valid UTF-8")))
        };
        match flag {
            "-h" | "--help" if inline.is_none() => return Ok(Parsed::Help),
            "--build" if inline.is_none() => return Ok(Parsed::Build),
            "--version" if inline.is_none() => return Ok(Parsed::Version),
            "-e" if inline.is_none() => {
                let cmd: Vec<OsString> = it.by_ref().collect();
                if cmd.is_empty() {
                    return Err(CliError("-e needs a command".into()));
                }
                cli.exec = Some(cmd);
            }
            "--working-directory" => {
                cli.working_directory = Some(PathBuf::from(value(&mut it)?));
            }
            "-o" | "--override" => {
                let v = text(value(&mut it)?, flag)?;
                cli.overrides.push(v);
            }
            "--app-id" => cli.app_id = Some(text(value(&mut it)?, flag)?),
            "--title" => cli.title = Some(text(value(&mut it)?, flag)?),
            "--standalone" if inline.is_none() => cli.standalone = true,
            _ => return Err(CliError(format!("unknown argument {a:?}"))),
        }
    }
    Ok(Parsed::Run(cli))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Result<Parsed, CliError> {
        parse(a.iter().map(OsString::from).collect())
    }

    fn run(a: &[&str]) -> Cli {
        match p(a) {
            Ok(Parsed::Run(c)) => c,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn cde_spawn_shape() {
        let c = run(&[
            "-o",
            "pad=6x6 center",
            "--working-directory",
            "/tmp",
            "-e",
            "sh",
            "-c",
            "echo --help",
        ]);
        assert_eq!(c.overrides, ["pad=6x6 center"]);
        assert_eq!(c.working_directory, Some("/tmp".into()));
        assert_eq!(c.exec.unwrap(), ["sh", "-c", "echo --help"]);
    }

    #[test]
    fn equals_forms_and_repeats() {
        let c = run(&[
            "--working-directory=/a",
            "--override=x=1",
            "-o",
            "y=2",
            "--app-id=a.b",
            "--title",
            "T",
        ]);
        assert_eq!(c.working_directory, Some("/a".into()));
        assert_eq!(c.overrides, ["x=1", "y=2"]);
        assert_eq!(
            (c.app_id.as_deref(), c.title.as_deref()),
            (Some("a.b"), Some("T"))
        );
        assert_eq!(c.exec, None);
    }

    #[test]
    fn info_flags_and_verbatim_exec() {
        assert_eq!(p(&["--help"]), Ok(Parsed::Help));
        assert_eq!(p(&["-h"]), Ok(Parsed::Help));
        assert_eq!(p(&["--build"]), Ok(Parsed::Build));
        assert_eq!(p(&["--title", "t", "--version"]), Ok(Parsed::Version));
        // After -e the words belong to the child, even --help and -o.
        let c = run(&["-e", "vim", "--help", "-o", "x"]);
        assert_eq!(c.exec.unwrap(), ["vim", "--help", "-o", "x"]);
        assert!(c.overrides.is_empty());
    }

    #[test]
    fn errors_are_usage_errors() {
        assert!(p(&["-e"]).is_err());
        assert!(p(&["--bogus"]).is_err());
        assert!(p(&["--title"]).is_err());
        assert!(p(&["-o"]).is_err());
        assert!(p(&["--help=1"]).is_err());
        assert!(p(&["stray"]).is_err());
    }

    #[test]
    fn daemon_subcommand_and_standalone_flag() {
        assert_eq!(p(&["daemon"]), Ok(Parsed::Daemon));
        assert_eq!(p(&["daemon", "--help"]), Ok(Parsed::DaemonHelp));
        assert!(p(&["daemon", "x"]).is_err());
        // Only the first argument can be the subcommand.
        assert!(p(&["--title", "t", "daemon"]).is_err());
        assert!(run(&["--standalone"]).standalone);
        assert_eq!(run(&["-e", "daemon"]).exec.unwrap(), ["daemon"]);
    }
}
