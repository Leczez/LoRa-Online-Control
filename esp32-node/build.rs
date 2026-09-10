fn main() {
    embuild::espidf::sysenv::output();
    emit_git_sha();
}

/// Embeds a git-describe string via GIT_SHA (read at runtime through
/// env!("GIT_SHA") in src/version.rs), same purpose as lora-server's and
/// roc-server's own build.rs — see either for the full rationale. This
/// crate is simpler: it's flashed by building directly on a dev machine
/// (no cross/Docker container in the way), so .git is always right there
/// and this can just run `git describe` unconditionally.
fn emit_git_sha() {
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
