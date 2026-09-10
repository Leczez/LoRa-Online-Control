fn main() {
    embuild::espidf::sysenv::output();
    emit_version();
}

/// Embeds SEMVER (from the repo-root VERSION file) and GIT_SHA (from `git
/// describe`), read at runtime via the VERSION const in main.rs — same
/// purpose as lora-server's and roc-server's own build.rs, see either for
/// the full rationale on why VERSION is the source of truth over
/// CARGO_PKG_VERSION. This crate is simpler on the GIT_SHA half: it's
/// flashed by building directly on a dev machine (no cross/Docker container
/// in the way), so .git is always right there and this can just run `git
/// describe` unconditionally.
fn emit_version() {
    let semver = std::fs::read_to_string("../VERSION")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=SEMVER={semver}");
    println!("cargo:rerun-if-changed=../VERSION");

    let sha = std::process::Command::new("git")
        .args(["describe", "--always", "--dirty=.dirty", "--abbrev=8"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_SHA={sha}");
}
