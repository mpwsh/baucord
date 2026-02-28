fn main() {
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rustc-link-lib=static=libx264");

        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/icons/icon.ico");
        res.compile().unwrap();
    }
}
