//! The macOS app bundle, for the updater (macos/README.md): which
//! `.app` the running copy lives in, how a downloaded one is unpacked,
//! and how it takes the old one's place.
//!
//! An installed `baucord.app` is updated whole, never by swapping its
//! executable: the bundle is signed as one piece, and a binary swapped
//! in on its own would break that signature and, with it, the
//! permissions macOS ties to it (microphone, screen recording,
//! Accessibility). Nothing here is macOS-only code, so it builds and is
//! tested everywhere; only the updater's macOS path calls it.

use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

/// The bundle's folder name in the update zip
/// (`baucord-v<version>-macos.zip`, made by `ditto --keepParent`).
pub const BUNDLE_NAME: &str = "baucord.app";

/// The `.app` folder the executable `exe` runs from — `…/baucord.app`
/// for `…/baucord.app/Contents/MacOS/baucord` — or `None` for a bare
/// binary (the zip installs, and every other platform).
#[must_use]
pub fn bundle_root(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let root = contents.parent()?;
    let is_bundle = macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && root.extension()? == "app";
    is_bundle.then(|| root.to_path_buf())
}

/// Unpack the whole zip, folders and all, into `dest`. Unix modes are
/// kept (the executable bit), and symbolic links stay links, so a
/// signed bundle comes out as it went in. macOS archive litter
/// (`__MACOSX/`, `._*`) is skipped, and any entry that would land
/// outside `dest` refuses the whole archive.
///
/// # Errors
/// The archive cannot be read, an entry is unsafe, or a file cannot be
/// written; the message says which.
pub fn extract_tree(zip_path: &Path, dest: &Path) -> Result<(), String> {
    let file = fs::File::open(zip_path)
        .map_err(|e| format!("Failed to open {}: {e}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("Failed to read zip: {e}"))?;
    fs::create_dir_all(dest).map_err(|e| format!("Failed to create {}: {e}", dest.display()))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("Failed to read zip entry {i}: {e}"))?;
        let Some(relative) = entry.enclosed_name() else {
            return Err(format!("Unsafe path in update zip: {}", entry.name()));
        };
        if is_litter(&relative) {
            continue;
        }
        let out = dest.join(&relative);
        if entry.is_dir() {
            fs::create_dir_all(&out)
                .map_err(|e| format!("Failed to create {}: {e}", out.display()))?;
            continue;
        }
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
        }
        let mode = entry.unix_mode();
        if mode.is_some_and(|mode| mode & 0o170_000 == 0o120_000) {
            let mut target = String::new();
            io::Read::read_to_string(&mut entry, &mut target)
                .map_err(|e| format!("Failed to read link {}: {e}", relative.display()))?;
            make_link(&target, &relative, &out)?;
            continue;
        }
        let mut file = fs::File::create(&out)
            .map_err(|e| format!("Failed to create {}: {e}", out.display()))?;
        io::copy(&mut entry, &mut file)
            .map_err(|e| format!("Failed to extract {}: {e}", out.display()))?;
        drop(file);
        set_mode(&out, mode)?;
    }
    Ok(())
}

/// Put `fresh` where `installed` is, keeping the old bundle at `aside`
/// (on the same volume, so both are renames). If the new one cannot be
/// moved in, the old one goes back: a failed update leaves a working
/// app. The caller deletes `aside` afterwards.
///
/// # Errors
/// Either rename failed; the installed app is then where it was.
pub fn swap_bundle(installed: &Path, fresh: &Path, aside: &Path) -> Result<(), String> {
    if aside.exists() {
        let _ = fs::remove_dir_all(aside);
    }
    fs::rename(installed, aside).map_err(|e| {
        format!(
            "Failed to move {} aside (is its folder writable?): {e}",
            installed.display()
        )
    })?;
    if let Err(e) = fs::rename(fresh, installed) {
        let _ = fs::rename(aside, installed);
        return Err(format!(
            "Failed to move the new app to {}: {e}",
            installed.display()
        ));
    }
    Ok(())
}

/// `__MACOSX/…` and `AppleDouble` `._name` files: what an archive made
/// with extended attributes carries beside the real files. Unpacked into
/// a bundle they would break its seal.
fn is_litter(path: &Path) -> bool {
    path.components().any(|part| {
        matches!(part, Component::Normal(name)
            if name == "__MACOSX" || name.to_string_lossy().starts_with("._"))
    })
}

/// A link inside the bundle, to somewhere inside the bundle: relative,
/// and climbing no higher than the link's own depth.
#[cfg(unix)]
fn make_link(target: &str, relative: &Path, out: &Path) -> Result<(), String> {
    let target_path = Path::new(target);
    let depth = relative.components().count().saturating_sub(1);
    let ups = target_path
        .components()
        .filter(|part| matches!(part, Component::ParentDir))
        .count();
    if target_path.is_absolute() || ups > depth {
        return Err(format!(
            "Unsafe link in update zip: {} -> {target}",
            relative.display()
        ));
    }
    std::os::unix::fs::symlink(target_path, out)
        .map_err(|e| format!("Failed to create link {}: {e}", out.display()))
}

#[cfg(not(unix))]
fn make_link(target: &str, relative: &Path, _out: &Path) -> Result<(), String> {
    Err(format!(
        "Links are not supported here: {} -> {target}",
        relative.display()
    ))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: Option<u32>) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let Some(mode) = mode else {
        return Ok(());
    };
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))
        .map_err(|e| format!("Failed to set mode on {}: {e}", path.display()))
}

// The Unix version can fail; the callers' `?` needs one signature.
#[cfg(not(unix))]
#[expect(clippy::unnecessary_wraps)]
fn set_mode(_path: &Path, _mode: Option<u32>) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    #[test]
    fn finds_the_bundle_of_its_executable() {
        assert_eq!(
            bundle_root(Path::new(
                "/Applications/baucord.app/Contents/MacOS/baucord"
            )),
            Some(PathBuf::from("/Applications/baucord.app"))
        );
        // Renamed by the user: still the bundle it runs from.
        assert_eq!(
            bundle_root(Path::new(
                "/Users/a/Apps/Baucord 2.app/Contents/MacOS/baucord"
            )),
            Some(PathBuf::from("/Users/a/Apps/Baucord 2.app"))
        );
    }

    #[test]
    fn a_bare_binary_has_no_bundle() {
        assert_eq!(bundle_root(Path::new("/Users/a/baucord/baucord")), None);
        assert_eq!(
            bundle_root(Path::new("/opt/x/Contents/MacOS/baucord")),
            None
        );
        assert_eq!(bundle_root(Path::new("C:\\baucord\\baucord.exe")), None);
        assert_eq!(bundle_root(Path::new("baucord")), None);
    }

    #[test]
    fn litter_is_recognised() {
        assert!(is_litter(Path::new("__MACOSX/baucord.app/._Info.plist")));
        assert!(is_litter(Path::new("baucord.app/Contents/._Info.plist")));
        assert!(!is_litter(Path::new("baucord.app/Contents/Info.plist")));
    }

    /// A scratch folder under the system's temp, gone when dropped.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("baucord-bundle-test-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_zip(path: &Path, entries: &[(&str, Option<&[u8]>, u32)]) {
        let file = fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (name, body, mode) in entries {
            let options = zip::write::SimpleFileOptions::default().unix_permissions(*mode);
            match body {
                None => zip.add_directory(*name, options).unwrap(),
                Some(body) if mode & 0o170_000 == 0o120_000 => zip
                    .add_symlink(*name, std::str::from_utf8(body).unwrap(), options)
                    .unwrap(),
                Some(body) => {
                    zip.start_file(*name, options).unwrap();
                    zip.write_all(body).unwrap();
                }
            }
        }
        zip.finish().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unpacks_a_bundle_with_modes_and_links() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = Scratch::new("unpack");
        let zip_path = scratch.0.join("update.zip");
        write_zip(
            &zip_path,
            &[
                ("baucord.app/", None, 0o755),
                ("baucord.app/Contents/", None, 0o755),
                ("baucord.app/Contents/Info.plist", Some(b"plist"), 0o644),
                ("baucord.app/Contents/MacOS/baucord", Some(b"exe"), 0o755),
                ("baucord.app/Contents/Current", Some(b"MacOS"), 0o120_755),
                ("__MACOSX/baucord.app/._Info.plist", Some(b"junk"), 0o644),
            ],
        );
        let dest = scratch.0.join("out");
        extract_tree(&zip_path, &dest).unwrap();

        let exe = dest.join("baucord.app/Contents/MacOS/baucord");
        assert_eq!(fs::read(&exe).unwrap(), b"exe");
        assert_eq!(
            fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let link = dest.join("baucord.app/Contents/Current");
        assert_eq!(fs::read_link(&link).unwrap(), PathBuf::from("MacOS"));
        assert!(!dest.join("__MACOSX").exists());
    }

    #[test]
    fn refuses_entries_that_escape() {
        let scratch = Scratch::new("escape");
        let zip_path = scratch.0.join("evil.zip");
        write_zip(&zip_path, &[("../outside", Some(b"x"), 0o644)]);
        let err = extract_tree(&zip_path, &scratch.0.join("out")).unwrap_err();
        assert!(err.contains("Unsafe path"), "{err}");
        assert!(!scratch.0.join("outside").exists());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_links_that_escape() {
        let scratch = Scratch::new("link");
        let zip_path = scratch.0.join("evil.zip");
        write_zip(
            &zip_path,
            &[("baucord.app/Contents/x", Some(b"../../../etc"), 0o120_755)],
        );
        let err = extract_tree(&zip_path, &scratch.0.join("out")).unwrap_err();
        assert!(err.contains("Unsafe link"), "{err}");
    }

    #[test]
    fn swaps_and_keeps_the_old_one_aside() {
        let scratch = Scratch::new("swap");
        let installed = scratch.0.join("baucord.app");
        let fresh = scratch.0.join("new/baucord.app");
        let aside = scratch.0.join("old.app");
        fs::create_dir_all(&installed).unwrap();
        fs::write(installed.join("v"), "old").unwrap();
        fs::create_dir_all(&fresh).unwrap();
        fs::write(fresh.join("v"), "new").unwrap();

        swap_bundle(&installed, &fresh, &aside).unwrap();
        assert_eq!(fs::read_to_string(installed.join("v")).unwrap(), "new");
        assert_eq!(fs::read_to_string(aside.join("v")).unwrap(), "old");
        assert!(!fresh.exists());
    }

    #[test]
    fn a_failed_swap_puts_the_old_one_back() {
        let scratch = Scratch::new("swapfail");
        let installed = scratch.0.join("baucord.app");
        let aside = scratch.0.join("old.app");
        fs::create_dir_all(&installed).unwrap();
        fs::write(installed.join("v"), "old").unwrap();

        // There is no new bundle to move in.
        let err = swap_bundle(&installed, &scratch.0.join("missing.app"), &aside).unwrap_err();
        assert!(err.contains("Failed to move the new app"), "{err}");
        assert_eq!(fs::read_to_string(installed.join("v")).unwrap(), "old");
        assert!(!aside.exists());
    }
}
