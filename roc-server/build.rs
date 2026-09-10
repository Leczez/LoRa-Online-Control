// roc-server/build.rs
//
// Embeds two things into the binary, read at runtime through src/version.rs:
//
//   - SEMVER, from the repo-root VERSION file (../VERSION from here) — the
//     one human-editable number, bumped by hand for releases. Reading it at
//     build time instead of hardcoding CARGO_PKG_VERSION means VERSION is
//     the actual single source of truth: `cat VERSION` and what a deployed
//     binary reports can never drift apart. Falls back to CARGO_PKG_VERSION
//     if the file is missing rather than failing the build — see Dockerfile
//     for how it reaches the Docker build context (explicitly COPYed in,
//     same as Cargo.toml/Cargo.lock).
//   - GIT_SHA, a git-describe string — lets a deployed binary answer "what
//     commit is this, and was the tree dirty when it was built" (VERSION
//     alone doesn't move between commits). Two build environments need to
//     reach it differently:
//       - `cargo build`/`cargo test` on a dev machine: .git is right there,
//         so just run `git describe` directly.
//       - `docker compose build` (scripts/deploy-roc-server.sh, this
//         crate's only deploy path): the .dockerignore excludes .git from
//         the build context entirely (and even on a remote build,
//         deploy-roc-server.sh's rsync excludes it too before syncing
//         source over), so there's no working .git inside the image at
//         all. deploy-roc-server.sh computes GIT_SHA on the host, where
//         git does work, and forwards it in as a Docker build ARG (see
//         Dockerfile's `ARG GIT_SHA` / `ENV GIT_SHA`) — so this script
//         only needs to prefer an already-set GIT_SHA over trying to run
//         git itself. Falls back to "unknown" if neither is available.
fn main() {
    let semver = std::fs::read_to_string("../VERSION")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=SEMVER={semver}");
    println!("cargo:rerun-if-changed=../VERSION");

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
