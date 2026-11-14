// build.rs — embed the application manifest so Windows grants us
// per-monitor DPI v2 awareness and UAC "asInvoker" execution level.

fn main() {
    // Only meaningful on MSVC targets
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // Embed manifest.xml as a Win32 resource so Explorer and DWM read it.
        println!("cargo:rerun-if-changed=manifest.xml");
        // Use windres / rc.exe style embedding via the `windows` build helper.
        // We write a minimal .rc file and compile it.
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let manifest_path = format!("{manifest_dir}/manifest.xml").replace('\\', "/");
        let out_dir = std::env::var("OUT_DIR").unwrap();
        let rc_path = format!("{out_dir}/manifest.rc");
        std::fs::write(
            &rc_path,
            format!("1 24 \"{manifest_path}\"\n"),  // RT_MANIFEST = 24
        )
        .unwrap();

        // Cargo will pick up any .res file placed in OUT_DIR if we emit the
        // right cargo directive, but the simplest path on MSVC is to call
        // rc.exe ourselves.  If rc.exe isn't on PATH we fall back gracefully.
        let res_path = format!("{out_dir}/manifest.res");
        let status = std::process::Command::new("rc.exe")
            .args(["/nologo", "/fo", &res_path, &rc_path])
            .status();

        if status.map(|s| s.success()).unwrap_or(false) {
            println!("cargo:rustc-link-arg={res_path}");
        }
        // If rc.exe is unavailable the binary still works; DPI awareness is
        // also set programmatically in win32_utils via SetProcessDpiAwarenessContext.
    }
}
