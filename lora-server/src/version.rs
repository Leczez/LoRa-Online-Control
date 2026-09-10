// lora-server/src/version.rs
//
// `<semver>+<git-sha>[.dirty]`, e.g. "0.1.0+a1b2c3d4" or
// "0.1.0+a1b2c3d4.dirty" — valid semver 2.0 build-metadata syntax (a single
// leading `+`, dot-separated identifiers after it). GIT_SHA is embedded by
// build.rs; see that file for why it isn't just `git describe` run here
// directly (cross-compilation doesn't have a working .git to ask).
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("GIT_SHA"));

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the semver 2.0 build-metadata grammar this is meant to
    /// satisfy: exactly one `+`, and a non-empty git-sha component after it
    /// (build.rs falls back to "unknown" rather than emitting an empty
    /// GIT_SHA, but this catches a regression in that fallback too).
    #[test]
    fn test_version_has_exactly_one_plus_and_nonempty_parts() {
        let parts: Vec<&str> = VERSION.split('+').collect();
        assert_eq!(parts.len(), 2, "VERSION should have exactly one '+': {VERSION}");
        assert!(!parts[0].is_empty() && !parts[1].is_empty(), "VERSION had an empty part: {VERSION}");
    }
}
