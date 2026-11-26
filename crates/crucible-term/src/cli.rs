//! The command line CDE spawns the terminal through: a pure parser over the argument vector.
//!
//! ```text
//! cterm [--standalone] [--working-directory DIR] [-o KEY=VALUE]... [--app-id ID] [--title T] [-e CMD [ARG]...]
//! cterm daemon [--help]
//! cterm ls [HOST] | cterm attach [HOST:]SESSION | cterm kill [HOST:]SESSION
//! ```
//!
//! Everything after `-e` is the child's argv, verbatim (CDE's `exec-flag`), so `--help` after
//! `-e` belongs to the child. `--help`/`-h`, `--build` and `--version` answer without reading
//! any config. An unknown flag, a flag missing its value, or `-e` with nothing after it is a
//! [`CliError`]; the caller prints usage on stderr and exits 2.

use std::ffi::OsString;
use std::path::PathBuf;

/// The usage text (`--help` prints it on stdout; errors print it on stderr).
pub const USAGE: &str = "usage: cterm [OPTIONS] [-e CMD [ARG]...]\n\nOpen a terminal window running CMD (default: $SHELL, fallback /bin/sh); the exit status is\nthe child's (128 + signal when killed).\n\n  -e CMD [ARG]...          run CMD with its arguments; everything after -e is the argv\n  --working-directory DIR  start the child in DIR (a missing DIR warns and uses the current dir)\n  -o, --override KEY=VAL   override a config key (repeatable), e.g. -o pad=6x6\\ center\n                           -o font=Mono:size=12 -o colors.alpha=0.9 -o scrollback=5000\n  --app-id ID              xdg app id (default crucible-term)\n  --title TITLE            initial window title (the child can still change it)\n  --standalone             run this window in-process even when config `mode` is daemon/auto\n  --build                  print the tool-scoped build id (40-hex commit, `-dirty` if modified)\n  --version                print `cterm <semver> (<build id>)`\n  -h, --help               print this usage and exit 0\n\nSubcommands:\n  daemon                   run the per-user window daemon (see `cterm daemon --help`)\n  ls [HOST]                list the daemon's sessions (see `cterm ls --help`)\n  attach [HOST:]SESSION    view and type into a session from this terminal\n  kill [HOST:]SESSION      close a session as a window close does\n\nDaemon mode reaches $XDG_RUNTIME_DIR/cterm/daemon.sock; config `daemon-start` is `service`\n(default: connect only, a missing socket runs standalone) or `spawn` (start the daemon if absent).\n\nConfig: $XDG_CONFIG_HOME/crucible-term/config.kdl (fallback ~/.config/crucible-term/config.kdl).\n";

/// The `cterm daemon --help` text.
pub const DAEMON_USAGE: &str = "usage: cterm daemon [-h | --help]\n\nRun the per-user crucible-term daemon in the foreground: one daemon serves the user (not one per\ndisplay) and owns the PTYs and windows of every `cterm` thin client (config `mode daemon`, or\n`auto` with a CDE grant). It starts with no Wayland connection and connects to the display the\nfirst client's open names; while connected, an open from another display is refused\n`other-display` and that client runs standalone. Normally the systemd user unit cterm.service\nruns it (socket-activated by cterm.socket); config `daemon-start \"spawn\"` makes clients start it\nthemselves instead.\n\nSocket: $XDG_RUNTIME_DIR/cterm/daemon.sock (lock: daemon.lock beside it); a listener passed by\nsystemd (LISTEN_FDS) is used when present, else the daemon binds the path.\nLog:    journalctl --user -u cterm for the service; $XDG_STATE_HOME/crucible-term/daemon.log when\n        a client spawned it.\nIt stays resident after its last window; config `daemon-linger N` exits after N idle seconds.\nIf its Wayland connection is lost, sessions stay live and it retries that display every second;\nthe next open from any display while it is lost is adopted instead. After a reconnect, config\n`detached-timeout N` closes sessions no client reclaims within N seconds.\nKilling the daemon closes every window. With config `scrollback-persist #true` it keeps a record of\neach keyed session in $XDG_STATE_HOME/crucible-term/sessions/ and replays it on the next open\nwith that key; SIGTERM or SIGHUP then saves every session and exits 0 (SIGKILL saves nothing).\n";

/// The `cterm ls --help` text.
pub const LS_USAGE: &str = "usage: cterm ls [HOST]\n\nList the sessions of the per-user daemon, one line each: id, short key (first 8 hex, or -),\npid, COLSxROWS, window or detached, viewer count, title, working directory. An empty list\nprints nothing. With HOST, runs `ssh -- HOST cterm ls` and relays its exit status.\n\nThe daemon is reached at $XDG_RUNTIME_DIR/cterm/daemon.sock (socket activation may start it);\nno daemon is spawned and nothing runs standalone. Exit 0, 1 when the daemon is unreachable,\n2 on a usage error.\n";

/// The `cterm attach --help` text.
pub const ATTACH_USAGE: &str = "usage: cterm attach [HOST:]SESSION\n\nView a running session in this terminal and type into it, alongside its window and any other\nviewer. SESSION is a decimal id or a hex key prefix (see `cterm ls`). Needs a terminal on\nstdin and stdout. The session follows the size of the viewer that typed last.\n\nPrefix key (config `attach-prefix`, default \"C-\\\\\"): Ctrl-\\ then d detaches; Ctrl-\\ twice\nsends one literal Ctrl-\\; Ctrl-\\ then any other key sends both.\n\nExit status: 0 on detach, the child's code when the session exits, 1 on a refusal or a lost\ndaemon, 2 on a usage error or without a terminal.\n\nWith HOST, runs `ssh -t -- HOST cterm attach SESSION`, relays its exit status, and resets this\nterminal when ssh ends, however it ended. The far side needs `cterm` on the PATH of a\nnon-interactive ssh command. A hand-typed `ssh -t HOST cterm attach SESSION` whose connection\ndrops leaves this terminal as ssh left it (possibly on the alternate screen, keys remapped):\nrun `reset` to recover.\n";

/// The `cterm kill --help` text.
pub const KILL_USAGE: &str = "usage: cterm kill [HOST:]SESSION\n\nClose a session as a window close does (SIGHUP, then the grace) and wait for its exit.\nSESSION is a decimal id or a hex key prefix (see `cterm ls`). With HOST, runs\n`ssh -- HOST cterm kill SESSION` and relays its exit status.\n\nExit 0 when the session exited, 1 when the daemon refused (no such session, ambiguous) or is\nunreachable, 2 on a usage error.\n";

/// Where an `attach` or `kill` goes: a session token, on this host or (via ssh) on `host`.
#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    /// A validated ssh host (`[A-Za-z0-9._@-]+`, not starting with `-`).
    pub host: Option<String>,
    /// A validated session token (`[0-9A-Fa-f]+`).
    pub session: String,
}

/// The attach-family subcommands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sub {
    /// `cterm ls`.
    Ls,
    /// `cterm attach`.
    Attach,
    /// `cterm kill`.
    Kill,
}

impl Sub {
    /// Its `--help` text.
    pub fn usage(self) -> &'static str {
        match self {
            Sub::Ls => LS_USAGE,
            Sub::Attach => ATTACH_USAGE,
            Sub::Kill => KILL_USAGE,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Sub::Ls => "ls",
            Sub::Attach => "attach",
            Sub::Kill => "kill",
        }
    }

    fn of(word: &OsString) -> Option<Sub> {
        match word.to_str()? {
            "ls" => Some(Sub::Ls),
            "attach" => Some(Sub::Attach),
            "kill" => Some(Sub::Kill),
            _ => None,
        }
    }
}

/// The usage text a parse error of `args` prints.
pub fn usage_of(args: &[OsString]) -> &'static str {
    args.first().and_then(Sub::of).map_or(USAGE, Sub::usage)
}

/// A conservative ssh host: `[A-Za-z0-9._@-]+`, not starting with `-`.
pub fn valid_host(h: &str) -> bool {
    !h.is_empty()
        && !h.starts_with('-')
        && h.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._@-".contains(&b))
}

/// A session token: hex or decimal digits only.
pub fn valid_session(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit())
}

const HOST_RULE: &str = "want [A-Za-z0-9._@-]+, not starting with -";

fn target(word: &str) -> Result<Target, String> {
    let (host, session) = match word.split_once(':') {
        Some((h, _)) if !valid_host(h) => return Err(format!("bad host {h:?} ({HOST_RULE})")),
        Some((h, s)) => (Some(h.to_string()), s),
        None => (None, word),
    };
    if !valid_session(session) {
        return Err(format!(
            "bad session {session:?} (want a decimal id or a hex key prefix)"
        ));
    }
    Ok(Target {
        host,
        session: session.to_string(),
    })
}

/// Parse the words after `ls`, `attach` or `kill`.
fn parse_sub(sub: Sub, rest: &[OsString]) -> Result<Parsed, CliError> {
    let name = sub.name();
    let words = rest
        .iter()
        .map(|a| {
            a.to_str()
                .ok_or_else(|| CliError(format!("{name}: argument {a:?} is not valid UTF-8")))
        })
        .collect::<Result<Vec<&str>, _>>()?;
    let err = |m: String| CliError(format!("{name}: {m}"));
    match (sub, words.as_slice()) {
        (_, ["-h" | "--help"]) => Ok(Parsed::SubHelp(sub)),
        (_, ["--build"]) => Ok(Parsed::Build),
        (_, ["--version"]) => Ok(Parsed::Version),
        (Sub::Ls, []) => Ok(Parsed::Ls(None)),
        (Sub::Ls, [h]) if valid_host(h) => Ok(Parsed::Ls(Some((*h).to_string()))),
        (Sub::Ls, [h]) => Err(err(format!("bad host {h:?} ({HOST_RULE})"))),
        (Sub::Attach, [t]) => Ok(Parsed::Attach(target(t).map_err(err)?)),
        (Sub::Kill, [t]) => Ok(Parsed::Kill(target(t).map_err(err)?)),
        (_, []) => Err(err("needs [HOST:]SESSION".into())),
        _ => Err(err(format!("unexpected arguments {words:?}"))),
    }
}

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
    /// Print this subcommand's usage and exit 0.
    SubHelp(Sub),
    /// `cterm ls [HOST]`.
    Ls(Option<String>),
    /// `cterm attach [HOST:]SESSION`.
    Attach(Target),
    /// `cterm kill [HOST:]SESSION`.
    Kill(Target),
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
    // So are `ls`, `attach` and `kill`; after `-e` they are the child's words.
    if let Some(sub) = args.first().and_then(Sub::of) {
        return parse_sub(sub, &args[1..]);
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

    #[test]
    fn attach_family_subcommands() {
        assert_eq!(p(&["ls"]), Ok(Parsed::Ls(None)));
        assert_eq!(p(&["ls", "u@h.x"]), Ok(Parsed::Ls(Some("u@h.x".into()))));
        assert_eq!(p(&["attach", "--help"]), Ok(Parsed::SubHelp(Sub::Attach)));
        assert_eq!(p(&["kill", "--version"]), Ok(Parsed::Version));
        assert_eq!(p(&["ls", "--build"]), Ok(Parsed::Build));
        let t = |host: Option<&str>, s: &str| Target {
            host: host.map(Into::into),
            session: s.into(),
        };
        assert_eq!(p(&["attach", "ab12"]), Ok(Parsed::Attach(t(None, "ab12"))));
        assert_eq!(p(&["kill", "h-1:7"]), Ok(Parsed::Kill(t(Some("h-1"), "7"))));
        for bad in [
            &["attach"][..],
            &["attach", "a", "b"],
            &["attach", "-oProxyCommand=x:1"],
            &["attach", "h:1;sh"],
            &["attach", "h x:1"],
            &["attach", ":1"],
            &["kill", "h:"],
            &["ls", "-h1"],
            &["ls", "a", "b"],
        ] {
            assert!(p(bad).is_err(), "{bad:?}");
        }
        // After -e, `ls` is the child's.
        assert_eq!(run(&["-e", "ls"]).exec.unwrap(), ["ls"]);
        assert_eq!(usage_of(&["kill".into()]), KILL_USAGE);
    }
}
