fn main() {
    // Build scripts run on the HOST, so `#[cfg(target_os = "windows")]`
    // here would test the build machine and silently skip icon
    // embedding when cross-compiling. CARGO_CFG_TARGET_OS is the
    // actual compilation target.
    println!("cargo:rerun-if-changed=../../assets/icons/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-lib=static=libx264");

        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/icons/icon.ico");
        res.compile()
            .expect("embed Windows resources (assets/icons/icon.ico)");
    }
}
