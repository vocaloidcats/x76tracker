fn main() {
    slint_build::compile("ui/app.slint").expect("Slint UI failed to compile");

    // Icon and file details shown in Explorer (Properties > Details).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico")
            .set("FileDescription", "x76 Elo Tracker (unofficial)")
            .set("ProductName", "x76 Elo Tracker")
            .set("LegalCopyright", "MIT License");
        if let Err(e) = res.compile() {
            println!("cargo:warning=could not embed the app icon: {e}");
        }
    }
}
