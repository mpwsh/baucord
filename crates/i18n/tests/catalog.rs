//! The catalogs against the code: every message is one the app can
//! show, and the three languages hold the same messages.
//!
//! A message is used when its id is a string literal somewhere in the
//! Rust sources (`t!("id")`, or an id picked into a variable first), or
//! when it is a `Tr` property (`ui/slint/tr.slint`, filled from the
//! message of the same name by `frontend/build.rs`) that a `.slint`
//! file reads. Thirty-three messages and twelve properties had been
//! left behind by code that moved on, in each of three files, and
//! translators kept translating them (the review's dead-code list).

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/i18n lives two levels down")
        .to_path_buf()
}

/// Every file under `dir` with this extension, build output aside.
fn files(dir: &Path, extension: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n != "target") {
                files(&path, extension, out);
            }
        } else if path.extension().is_some_and(|x| x == extension) {
            out.push(path);
        }
    }
}

fn is_id(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_lowercase())
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The message ids of one catalog, in file order.
fn ids_of(locale: &str) -> Vec<String> {
    let path = workspace().join("crates/i18n/locales").join(locale);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(id, _)| id.trim_end())
        .filter(|id| is_id(id))
        .map(str::to_string)
        .collect()
}

/// Everything between two quotes in the Rust sources that could be an
/// id. Generous on purpose: a false "used" only keeps a message.
fn rust_literals() -> BTreeSet<String> {
    let root = workspace();
    let mut sources = Vec::new();
    files(&root.join("crates"), "rs", &mut sources);
    files(&root.join("tools"), "rs", &mut sources);
    let mut found = BTreeSet::new();
    for path in sources {
        let text = fs::read_to_string(&path).unwrap_or_default();
        for literal in text.split('"').skip(1).step_by(2) {
            if is_id(literal) {
                found.insert(literal.to_string());
            }
        }
    }
    found
}

/// The `Tr` properties declared, and the ones some `.slint` file reads.
fn tr_properties() -> (BTreeSet<String>, BTreeSet<String>) {
    let root = workspace();
    let mut sources = Vec::new();
    files(&root.join("crates/ui/slint"), "slint", &mut sources);
    files(&root.join("tools"), "slint", &mut sources);
    let (mut declared, mut read) = (BTreeSet::new(), BTreeSet::new());
    for path in sources {
        let text = fs::read_to_string(&path).unwrap_or_default();
        if path.file_name().is_some_and(|n| n == "tr.slint") {
            for line in text.lines() {
                if let Some(name) = line.trim().strip_prefix("in property <string> ") {
                    declared.insert(name.trim_end_matches(';').trim().to_string());
                }
            }
            continue;
        }
        for after in text.split("Tr.").skip(1) {
            let name: String = after
                .chars()
                .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
                .collect();
            if is_id(&name) {
                read.insert(name);
            }
        }
    }
    (declared, read)
}

#[test]
fn the_three_catalogs_hold_the_same_messages() {
    let english = ids_of("en.ftl");
    let unique: BTreeSet<&String> = english.iter().collect();
    assert_eq!(unique.len(), english.len(), "en.ftl says a message twice");
    for locale in ["es-AR.ftl", "pt-BR.ftl"] {
        let theirs: BTreeSet<String> = ids_of(locale).into_iter().collect();
        let ours: BTreeSet<String> = english.iter().cloned().collect();
        let missing: Vec<&String> = ours.difference(&theirs).collect();
        let extra: Vec<&String> = theirs.difference(&ours).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "{locale}: missing {missing:?}, not in en.ftl {extra:?}"
        );
    }
}

#[test]
fn every_message_and_every_tr_property_is_used() {
    let (declared, read) = tr_properties();
    assert!(declared.len() > 100, "tr.slint was not found or is empty");
    let unread: Vec<&String> = declared.difference(&read).collect();
    assert!(
        unread.is_empty(),
        "`Tr` properties no .slint file reads — remove them from tr.slint: {unread:?}"
    );

    let literals = rust_literals();
    let unused: Vec<String> = ids_of("en.ftl")
        .into_iter()
        .filter(|id| !literals.contains(id) && !read.contains(id))
        .collect();
    assert!(
        unused.is_empty(),
        "messages nothing shows — remove them from every catalog: {unused:?}"
    );
}
