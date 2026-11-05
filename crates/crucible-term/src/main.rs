//! `crucible-term`: CDE's terminal emulator. With no arguments it spawns `$SHELL` (fallback
//! `/bin/sh`) on a PTY in a new Wayland window and exits with the shell's exit status. It
//! answers `--help`, `--build` and `--version`; every other argument is refused until WP-4's CLI.

mod font;
mod render;
mod window;

use std::ffi::OsString;
use std::process::ExitCode;

const USAGE: &str = "usage: crucible-term [--help | --build | --version]\n\nWith no arguments, open a terminal window running $SHELL (fallback /bin/sh) in the current\ndirectory; the exit status is the shell's.\n\n  --build                  print the tool-scoped build id (40-hex commit, `-dirty` if modified) and exit 0\n  --version                print `crucible-term <semver> (<build id>)` and exit 0\n  -h, --help               print this usage and exit 0\n\nAny other argument exits 2 (the full CLI arrives with plan 01 WP-4).\n";

/// The `--version` line, in the `"version": "semver"` format the fleet help gate matches.
fn version_line(version: &str, build_id: &str) -> String {
    format!("crucible-term {version} ({build_id})")
}

/// The shell to run: `$SHELL` if set and non-empty, else `/bin/sh`.
fn shell(var: Option<OsString>) -> OsString {
    var.filter(|s| !s.is_empty())
        .unwrap_or_else(|| OsString::from("/bin/sh"))
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--build") {
        println!("{}", env!("FLEET_BUILD_ID"));
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--version") {
        println!(
            "{}",
            version_line(env!("CARGO_PKG_VERSION"), env!("FLEET_BUILD_ID"))
        );
        return ExitCode::SUCCESS;
    }
    if let Some(a) = args.first() {
        eprintln!("crucible-term: unknown argument {a:?} (see --help)");
        return ExitCode::from(2);
    }
    let argv = vec![shell(std::env::var_os("SHELL"))];
    match window::run(&window::Config::default(), argv) {
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
        assert_eq!(version_line("0.0.0", "abc"), "crucible-term 0.0.0 (abc)");
    }

    #[test]
    fn shell_falls_back_to_bin_sh() {
        assert_eq!(shell(None), "/bin/sh");
        assert_eq!(shell(Some("".into())), "/bin/sh");
        assert_eq!(shell(Some("/bin/zsh".into())), "/bin/zsh");
    }
}
