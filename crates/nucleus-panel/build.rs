// Embed the git commit so panel/daemon version skew is detectable at
// runtime (CARGO_PKG_VERSION alone stays 0.1.0 across builds).
//
// No rerun-if-changed directives here on purpose: cargo's default is to
// rerun this script whenever any package source changes, which keeps
// NUCLEUS_BUILD_EPOCH accurate per compile. Pinning it to specific paths
// left the epoch stale and mis-ranked on-disk static assets against the
// embedded ones.
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
    // Compile timestamp: lets the panel prefer on-disk static assets only
    // when they are newer than this binary (a stale static/ dir from an old
    // deployment must never shadow the freshly embedded assets).
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=NUCLEUS_BUILD_EPOCH={epoch}");
}
