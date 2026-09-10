// lora-server/build.rs
//
// Embeds a git-describe string into the binary via the GIT_SHA env var
// (consumed at runtime through env!("GIT_SHA") in src/version.rs) — lets a
// deployed binary answer "what commit is this, and was the tree dirty when
// it was built" without cross-referencing deploy timestamps against git log
// by hand. Two build environments need to reach GIT_SHA differently:
//
//   - `cargo build`/`cargo test` on a dev machine: .git is right there, so
//     just run `git describe` directly.
//   - `cross build` (scripts/deploy.sh, this crate's only deploy path):
//     builds inside a Docker container that can't see the worktree's real
//     .git (a worktree's .git is a file pointing at an absolute host path
//     the container doesn't have). deploy.sh computes GIT_SHA on the host,
//     where git does work, and forwards it in via Cross.toml's
//     `[build.env] passthrough` — so this script only needs to prefer an
//     already-set GIT_SHA over trying to run git itself.
//
// Falls back to "unknown" if neither is available (e.g. a source tarball
// with no .git at all) rather than failing the build over a cosmetic value.
fn main() {
    let sha = std::env::var("GIT_SHA").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| {
        std::process::Command::new("git")
            .args(["describe", "--always", "--dirty=.dirty", "--abbrev=8"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    });
    println!("cargo:rustc-env=GIT_SHA={sha}");
    println!("cargo:rerun-if-env-changed=GIT_SHA");
}
