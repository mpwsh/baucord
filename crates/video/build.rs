fn main() {
    // FFmpeg static libs on Windows pull in DirectShow (dshow), Media Foundation (mfenc),
    // and COM symbols that aren't linked by default.
    if cfg!(target_os = "windows") {
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

