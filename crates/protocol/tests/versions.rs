//! The public crates move together (SESSION-GUIDE.md §4): their version
//! is what users, the server and the app stores see. Every platform
//! reads it from its own crate — the desktop's updater and About page
//! from `frontend`, the wire's `client_version` from this crate, the
//! server's from `server`, Android's `versionName` from `crates/android`,
//! iOS's `MARKETING_VERSION` from `crates/ios` — so bumping one alone
//! ships two numbers. 1.1.6 did: the apps said 1.1.6 and told the
//! server 1.1.5.

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

/// The `[package]` version: the first line that sets `version`
/// (dependencies say `version` only inside `{ … }`).
fn package_version(manifest: &Path) -> String {
    let text =
        std::fs::read_to_string(manifest).unwrap_or_else(|e| panic!("{}: {e}", manifest.display()));
    text.lines()
        .find_map(|line| line.strip_prefix("version = \""))
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_else(|| panic!("{}: no version line", manifest.display()))
        .to_string()
}

#[test]
fn public_crates_share_one_version() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("protocol lives in crates/");
    let ours = env!("CARGO_PKG_VERSION");
    let off: Vec<String> = PUBLIC
        .iter()
        .map(|name| (name, package_version(&crates.join(name).join("Cargo.toml"))))
        .filter(|(_, version)| version != ours)
        .map(|(name, version)| format!("{name} is {version}"))
        .collect();
    assert!(
        off.is_empty(),
        "the public crates move together (protocol is {ours}): {}",
        off.join(", ")
    );
}
