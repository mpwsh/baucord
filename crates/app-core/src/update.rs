//! Update-flow constants and URL building (ported from `ui/splash.rs`).
//!
//! The check/download/install *side effects* stay in the shells (a
//! phone updates through its store, not a zip) — but which endpoint to
//! ask and how versions compare is shared knowledge.

/// Remote endpoint that returns just the latest version string.
pub const VERSION_URL: &str = "https://baucord.mpw.sh/version";

/// Base URL for downloads — append the platform-specific filename.
pub const DOWNLOAD_BASE: &str = "https://baucord.mpw.sh";

/// Platform-specific zip filename suffix.
#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
pub const PLATFORM_SUFFIX: &str = "win-x64";

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const PLATFORM_SUFFIX: &str = "macos-arm64";

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub const PLATFORM_SUFFIX: &str = "macos-intel";

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const PLATFORM_SUFFIX: &str = "linux-x64";

/// Fallback for targets without published release archives (e.g. a
/// future linux-aarch64 build, or the mobile shells). Empty means "no
/// updater": the version check short-circuits to up-to-date instead of
/// prompting for a download that cannot exist.
#[cfg(not(any(
    all(target_os = "windows", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "macos", target_arch = "x86_64"),
    all(target_os = "linux", target_arch = "x86_64")
)))]
pub const PLATFORM_SUFFIX: &str = "";

/// Build the versioned download URL for a given version string.
#[must_use]
pub fn download_url(version: &str) -> String {
    format!("{DOWNLOAD_BASE}/baucord-v{version}-{PLATFORM_SUFFIX}.zip")
}

/// Is `remote` a newer version than `local`? Dotted-numeric compare,
/// missing components read as 0; any non-numeric component makes the
/// versions incomparable (→ false, don't nag the user).
#[must_use]
pub fn is_newer(remote: &str, local: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> {
        v.trim()
            .trim_start_matches('v')
            .split('.')
            .map(|c| c.parse::<u64>().ok())
            .collect()
    };
    match (parse(remote), parse(local)) {
        (Some(r), Some(l)) => {
            let len = r.len().max(l.len());
            for i in 0..len {
                let rv = r.get(i).copied().unwrap_or(0);
                let lv = l.get(i).copied().unwrap_or(0);
                if rv != lv {
                    return rv > lv;
                }
            }
            false
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_patch_wins() {
        assert!(is_newer("0.1.40", "0.1.39"));
    }

    #[test]
    fn equal_and_older_are_not_newer() {
        assert!(!is_newer("0.1.39", "0.1.39"));
        assert!(!is_newer("0.1.38", "0.1.39"));
    }

    #[test]
    fn component_count_mismatch_reads_missing_as_zero() {
        assert!(is_newer("0.2", "0.1.9"));
        assert!(!is_newer("0.1", "0.1.0"));
    }

    #[test]
    fn garbage_is_never_newer() {
        assert!(!is_newer("latest", "0.1.39"));
    }
}
