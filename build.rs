// Embedded manifest: apex requires administrator rights. The global hotkey
// is a low-level keyboard hook, and UIPI hides input from a medium-integrity
// process while an elevated window (Task Manager, an installer, regedit) has
// focus - so a non-elevated apex simply cannot be summoned over those. Running
// elevated is the only way the hotkey works everywhere. Apps launched from an
// elevated apex are handed back to Explorer so they still run unelevated (see
// src/launch.rs).
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="requireAdministrator" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
</assembly>
"#;

fn main() {
    println!("cargo:rerun-if-changed=assets/apex.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/apex.ico");
        res.set("ProductName", "Apex");
        res.set("FileDescription", "Apex - ultra-fast launcher");
        res.set_manifest(MANIFEST);
        res.compile().expect("failed to embed Windows resources");
    }
}
