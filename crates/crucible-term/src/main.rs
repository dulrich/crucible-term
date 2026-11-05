//! `crucible-term`: CDE's terminal emulator. Headless until plan 01 WP-2 brings the window; this
//! binary answers `--help`, `--build` and `--version` and refuses every other invocation.

use std::process::ExitCode;

const USAGE: &str = "usage: crucible-term [--help | --build | --version]\n\n  --build                  print the tool-scoped build id (40-hex commit, `-dirty` if modified) and exit 0\n  --version                print `crucible-term <semver> (<build id>)` and exit 0\n  -h, --help               print this usage and exit 0\n\nThe terminal window arrives with plan 01 WP-2; any other invocation exits 2.\n";

/// The `--version` line, in the `"version": "semver"` format the fleet help gate matches.
fn version_line(version: &str, build_id: &str) -> String {
    format!("crucible-term {version} ({build_id})")
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
    eprintln!("crucible-term: not yet implemented: window arrives with WP-2");
    ExitCode::from(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_has_the_semver_gate_shape() {
        assert_eq!(version_line("0.0.0", "abc"), "crucible-term 0.0.0 (abc)");
    }
}
