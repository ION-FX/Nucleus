// Embed the git commit so panel/daemon version skew is detectable at
// runtime (CARGO_PKG_VERSION alone stays 0.1.0 across builds).
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
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
