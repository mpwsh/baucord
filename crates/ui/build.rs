fn main() {
    // Compile the whole scene graph from the root file; it re-exports
    // the auxiliary windows (Settings, StreamViewer, Gallery).
    slint_build::compile("slint/main.slint").expect("slint build failed");
}
