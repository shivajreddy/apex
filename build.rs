fn main() {
    println!("cargo:rerun-if-changed=assets/apex.ico");
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/apex.ico");
        res.set("ProductName", "Apex");
        res.set("FileDescription", "Apex - ultra-fast launcher");
        res.compile().expect("failed to embed Windows resources");
    }
}
