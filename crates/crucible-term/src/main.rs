//! `cterm` (crate `crucible-term`): CDE's terminal emulator. It opens a Wayland window running `-e <cmd>` (or
//! `$SHELL`, fallback `/bin/sh`) on a PTY and exits with the child's exit status. The command
//! line is in [`cli`], the KDL configuration and `-o` overrides in [`config`].

mod cli;
mod client;
mod clipboard;
mod config;
mod daemon;
mod font;
mod outbox;
mod render;
mod selection;
mod window;
mod wire;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

/// The `--version` line, in the `"version": "semver"` format the fleet help gate matches.
fn version_line(version: &str, build_id: &str) -> String {
    format!("cterm {version} ({build_id})")
}

/// The shell to run: `$SHELL` if set and non-empty, else `/bin/sh`.
fn shell(var: Option<OsString>) -> OsString {
    var.filter(|s| !s.is_empty())
        .unwrap_or_else(|| OsString::from("/bin/sh"))
}

/// The working directory: the requested one if it is a directory (else a warning and the
/// current directory).
fn working_directory(requested: Option<PathBuf>) -> PathBuf {
    let current = || std::env::current_dir().unwrap_or_else(|_| "/".into());
    match requested {
        Some(d) if d.is_dir() => d,
        Some(d) => {
            eprintln!(
                "crucible-term: --working-directory {}: not a directory, using the current directory",
                d.display()
            );
            current()
        }
        None => current(),
    }
}

fn main() -> ExitCode {
    let cli = match cli::parse(std::env::args_os().skip(1).collect()) {
        Ok(cli::Parsed::Help) => {
            print!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(cli::Parsed::Build) => {
            println!("{}", env!("FLEET_BUILD_ID"));
            return ExitCode::SUCCESS;
        }
        Ok(cli::Parsed::Version) => {
            println!(
                "{}",
                version_line(env!("CARGO_PKG_VERSION"), env!("FLEET_BUILD_ID"))
            );
            return ExitCode::SUCCESS;
        }
        Ok(cli::Parsed::DaemonHelp) => {
            print!("{}", cli::DAEMON_USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(cli::Parsed::Daemon) => return daemon::run(),
        Ok(cli::Parsed::Run(c)) => c,
        Err(e) => {
            eprintln!("crucible-term: {e}\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    let path = config::config_path(&|k| std::env::var_os(k));
    let (settings, warnings) = config::load(
        path.as_deref(),
        &|p| std::fs::read_to_string(p),
        &cli.overrides,
    );
    for w in warnings {
        eprintln!("{w}");
    }
    let mut cfg = window::Config {
        settings,
        cwd: working_directory(cli.working_directory),
        ..window::Config::default()
    };
    if let Some(id) = &cli.app_id {
        cfg.app_id = id.clone();
    }
    if let Some(t) = &cli.title {
        cfg.title = t.clone();
    }
    let argv = cli
        .exec
        .unwrap_or_else(|| vec![shell(std::env::var_os("SHELL"))]);
    // The grant is for this spawn only: no child environment ever carries it.
    let grant = std::env::var_os(client::GRANT_VAR).map(|g| g.to_string_lossy().into_owned());
    let env = client::strip_grant(crucible_term_core::pty::inherited_env());
    if client::route(cfg.settings.mode, grant.is_some(), cli.standalone) == client::Route::Daemon {
        let open = wire::Open {
            argv: argv.clone(),
            cwd: cfg.cwd.clone(),
            env: env.clone(),
            overrides: cli.overrides,
            app_id: cli.app_id,
            title: cli.title,
            grant,
        };
        match client::run(open) {
            client::Outcome::Exit(code) => return ExitCode::from(u8::try_from(code).unwrap_or(1)),
            client::Outcome::Failed(e) => {
                eprintln!("crucible-term: daemon: {e}");
                return ExitCode::from(1);
            }
            client::Outcome::Fallback(w) => eprintln!("crucible-term: {w}; running standalone"),
        }
    }
    match window::run(&cfg, argv, env) {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(e) => {
            eprintln!("crucible-term: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_has_the_semver_gate_shape() {
        assert_eq!(version_line("0.0.0", "abc"), "cterm 0.0.0 (abc)");
    }

    #[test]
    fn shell_falls_back_to_bin_sh() {
        assert_eq!(shell(None), "/bin/sh");
        assert_eq!(shell(Some("".into())), "/bin/sh");
        assert_eq!(shell(Some("/bin/zsh".into())), "/bin/zsh");
    }
}
