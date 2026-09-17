// Build-time version string.
// Format: <branch>-<commit-count>-<short-hash>
// Set by build.rs via the LRMUX_VERSION env var.

pub const VERSION: &str = env!("LRMUX_VERSION");

/// The 7-character short hash at the end of VERSION.
pub fn short_hash() -> &'static str {
    VERSION.rsplit('-').next().unwrap_or(VERSION)
}
