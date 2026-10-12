//! The public crates move together (SESSION-GUIDE.md §4): their version
//! is what users, the server and the app stores see. Every platform
//! reads it — the desktop's updater and About page from `frontend`, the
//! wire's `client_version` from this crate, the server's from `server`,
//! Android's `versionName` and iOS's `MARKETING_VERSION` from the
//! workspace manifest — so two numbers in the tree ship two numbers.
//! 1.1.6 did: the apps said 1.1.6 and told the server 1.1.5.
//!
//! The number lives once, under `[workspace.package]` in the root
//! `Cargo.toml` (review P2-61), and each public crate inherits it. This
//! test holds them to that.

use std::path::Path;

/// The crates whose version is the product's.
const PUBLIC: [&str; 8] = [
    "protocol",
    "transport",
    "client",
    "server",
    "frontend",
    "desktop",
    "android",
    "ios",
];

fn read(manifest: &Path) -> String {
    std::fs::read_to_string(manifest).unwrap_or_else(|e| panic!("{}: {e}", manifest.display()))
}

#[test]
fn public_crates_share_one_version() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("protocol lives in crates/");
    let own: Vec<&str> = PUBLIC
        .iter()
        .copied()
        .filter(|name| {
            !read(&crates.join(name).join("Cargo.toml"))
                .lines()
                .any(|line| line == "version.workspace = true")
        })
        .collect();
    assert!(
        own.is_empty(),
        "these public crates set a version of their own instead of \
         `version.workspace = true`: {own:?}"
    );

    // The scripts and workflows that stamp a release (Gradle, the iOS
    // scripts, the release, Docker and web workflows) take the first
    // line of the root manifest that starts with `version = "`: there
    // must be exactly one, and it is ours.
    let root = read(&crates.parent().expect("the workspace").join("Cargo.toml"));
    let versions: Vec<&str> = root
        .lines()
        .filter_map(|line| line.strip_prefix("version = \""))
        .filter_map(|rest| rest.split('"').next())
        .collect();
    assert_eq!(versions, [env!("CARGO_PKG_VERSION")]);
}
