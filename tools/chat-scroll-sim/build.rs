//! Compiles the app's own `ChatPane` (crates/ui/slint/chat.slint) inside
//! a bare window. `CHAT_DIR` points at another copy of the .slint files
//! to compare a different version; `JUMP` is what its jump button
//! called, when that was not `pin-to-bottom()`.

use std::{collections::HashMap, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=CHAT_DIR");
    println!("cargo:rerun-if-env-changed=JUMP");
    let jump = std::env::var("JUMP").unwrap_or_else(|_| "pane.pin-to-bottom()".to_string());
    let here = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let dir = std::env::var("CHAT_DIR")
        .map_or_else(|_| here.join("../../crates/ui/slint"), PathBuf::from);
    println!("cargo:rerun-if-changed={}", dir.display());
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("set by cargo")).join("sim.slint");
    let sim = r#"
import { ChatPane } from "chat.slint";
export { State, Logic, ChatRow } from "globals.slint";
export { Tr } from "tr.slint";
export component TestWindow inherits Window {
    preferred-width: 400px;
    preferred-height: 640px;
    pane := ChatPane { width: 100%; height: 100%; }
    public function pin() { pane.pin-to-bottom(); }
    // What the jump button and the new-messages pill do.
    public function jump() { JUMP; }
    public function appended() -> bool { return pane.follow-appended(); }
    public pure function following() -> bool { return pane.following(); }
    public pure function gap() -> length { return pane.bottom-gap(); }
}
"#
    .replace("JUMP", &jump);
    std::fs::write(&out, sim).expect("write sim.slint");
    let libraries = HashMap::from([("lucide".to_string(), PathBuf::from(lucide_slint::lib()))]);
    let config = slint_build::CompilerConfiguration::new()
        .with_library_paths(libraries)
        .with_include_paths(vec![dir])
        .with_style("fluent".into());
    slint_build::compile_with_config(&out, config).expect("slint build");
}
