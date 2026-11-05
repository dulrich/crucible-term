// build.rs — fleet build-id helper (plans/fleet-build-version-gate.md, Decision 3).
// Copy-in template: save as the crate's `build.rs` (std only, no build-dependencies).
//
// Embeds FLEET_BUILD_ID at build time. A non-empty GIT_SHA env value wins verbatim
// (container builds without .git); otherwise the tool-scoped id is computed from
// CARGO_MANIFEST_DIR (tool root = nearest ancestor holding AGENTS.md, else the repo root).
//
// main.rs usage:
//   let args: Vec<String> = std::env::args().skip(1).collect();
//   if args.iter().any(|a| a == "--help" || a == "-h") { print_usage(); return; }
//   if args.iter().any(|a| a == "--build") { println!("{}", env!("FLEET_BUILD_ID")); return; }
//   if args.iter().any(|a| a == "--version") {
//       println!("{}", format!("{name} {} ({})", env!("CARGO_PKG_VERSION"), env!("FLEET_BUILD_ID"))); return;
//   }
// (non-semver entrypoints print env!("FLEET_BUILD_ID") for --version too)

#![allow(missing_docs)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("--no-optional-locks")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

fn build_id(dir: &Path) -> String {
    let dir = match std::fs::canonicalize(dir) {
        Ok(d) => d,
        Err(_) => return "unknown".into(),
    };
    let top = match git(&dir, &["rev-parse", "--show-toplevel"]) {
        Some(t) if !t.trim().is_empty() => PathBuf::from(t.trim()),
        _ => return "unknown".into(),
    };
    let mut tool = top.clone();
    let mut d: &Path = &dir;
    while d.starts_with(&top) {
        if std::fs::symlink_metadata(d.join("AGENTS.md")).is_ok() {
            tool = d.to_path_buf();
            break;
        }
        match d.parent() {
            Some(p) if d != top => d = p,
            _ => break,
        }
    }
    let tool_s = tool.to_string_lossy().into_owned();
    let sha = git(&top, &["log", "-1", "--format=%H", "--", &tool_s])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if sha.is_empty() {
        return "unknown".into();
    }
    let dirty = git(
        &top,
        &[
            "status",
            "--porcelain",
            "--untracked-files=no",
            "--",
            &tool_s,
        ],
    )
    .is_some_and(|s| !s.trim().is_empty());
    if dirty { format!("{sha}-dirty") } else { sha }
}

fn main() {
    println!("cargo:rerun-if-env-changed=GIT_SHA");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    for p in ["HEAD", "index"] {
        if let Some(gp) = git(
            &manifest,
            &["rev-parse", "--path-format=absolute", "--git-path", p],
        ) {
            println!("cargo:rerun-if-changed={}", gp.trim());
        }
    }
    let id = match std::env::var("GIT_SHA") {
        Ok(s) if !s.is_empty() => s,
        _ => build_id(&manifest),
    };
    println!("cargo:rustc-env=FLEET_BUILD_ID={id}");
}
