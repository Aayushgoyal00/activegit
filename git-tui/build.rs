//! Gives `activegit.exe` its icon and version details on Windows, so the
//! Start-menu / taskbar shortcut (`activegit --shortcut`) shows the app's
//! own icon instead of a generic console one. Nothing to do elsewhere.

fn main() {
    println!("cargo:rerun-if-changed=../assets/activegit.ico");
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(windows)]
    {
        if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
            let mut resource = winresource::WindowsResource::new();
            resource.set_icon("../assets/activegit.ico");
            resource.set("ProductName", "ActiveGit");
            resource.set("FileDescription", "ActiveGit");
            // The icon is a nicety: a machine without a resource compiler
            // (`rc.exe` or `windres`) still gets a working binary.
            if let Err(e) = resource.compile() {
                println!("cargo:warning=building activegit without its icon: {e}");
            }
        }
    }
}
