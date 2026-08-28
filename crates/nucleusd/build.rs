// Embed the git commit so panel/daemon version skew is detectable at
// runtime (CARGO_PKG_VERSION alone stays 0.1.0 across builds).
//
// No rerun-if-changed directives on purpose: cargo's default reruns this
// script whenever any package source changes, keeping NUCLEUS_GIT_SHA fresh
// per compile instead of pinned to a stale trigger path.
use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=NUCLEUS_GIT_SHA={sha}");
}
