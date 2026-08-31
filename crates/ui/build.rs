use std::{collections::HashMap, path::PathBuf};

fn main() {
    // `@lucide` resolves to lucide-slint's generated `lucide.slint`
    // (the `IconSet` global + `IconDisplay` component). Only the core
    // set is mounted; Lucide Lab is not used.
    let libraries = HashMap::from([("lucide".to_string(), PathBuf::from(lucide_slint::lib()))]);
    let config = slint_build::CompilerConfiguration::new().with_library_paths(libraries);

    // Compile the whole scene graph from the root file; it re-exports
    // the auxiliary windows (Settings, StreamViewer) and the tray.
    slint_build::compile_with_config("slint/main.slint", config).expect("slint build failed");
}
