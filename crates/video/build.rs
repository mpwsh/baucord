fn main() {
    // FFmpeg static libs on Windows pull in DirectShow (dshow), Media
    // Foundation (mfenc), and COM symbols that aren't linked by default.
    //
    // Checked via CARGO_CFG_TARGET_OS (the *target*), not cfg! (which in
    // a build script evaluates for the *host*) — matters now that this
    // crate cross-compiles for mobile targets.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // vcpkg's static ffmpeg[x264] port leaves libx264 for the
        // consumer to link (ffmpeg-sys-next only names the av* libs).
        // It used to live in the desktop binary's build script, which
        // meant this crate's own examples and tests could not link on
        // Windows (LNK2001 x264_*). A dependency's link directive
        // reaches every binary that links it, so here is where it goes.
        println!("cargo:rustc-link-lib=static=libx264");
        for lib in &[
            "ole32",    // COM runtime (CoCreateInstance, etc.)
            "oleaut32", // OLE Automation (SysAllocString, etc.)
            "strmiids", // DirectShow interface IIDs (IID_IBaseFilter, CLSID_FilterGraph, etc.)
            "uuid",     // Standard COM IIDs (IID_IUnknown, etc.)
            "mfuuid",   // Media Foundation IIDs (IID_IMFTransform, IID_ICodecAPI, etc.)
            "mfplat",   // Media Foundation platform APIs
            "secur32",  // SSPI (pulled in by some network components)
            "bcrypt",   // Crypto primitives
            "user32",   // Window management (avdevice needs it)
        ] {
            println!("cargo:rustc-link-lib={lib}");
        }
    }
}
