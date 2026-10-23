//! Compiles resources/resources.gresource.xml into the binary: the stylesheet and
//! the symbolic icons the app ships. Nothing is looked up on disk at run time,
//! so an installed binary and a source checkout behave the same.

fn main() {
    // The icons stay in the repository's assets folder, which the README and the
    // Flatpak repo file link to, so there is one copy of each.
    println!("cargo:rerun-if-changed=assets/icons");
    println!("cargo:rerun-if-changed=resources");
    glib_build_tools::compile_resources(&["resources", "assets/icons"], "resources/resources.gresource.xml", "mixtapes.gresource");

    // Room in the macOS binary's header for mac/bundle.sh, which rewrites every
    // library path to point into Mixtapes.app/Contents/Frameworks.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-arg-bins=-Wl,-headerpad_max_install_names");
    }

    // The exe's icon and file details on Windows. The target, not the host, decides.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rerun-if-changed=windows/mixtapes.rc");
        println!("cargo:rerun-if-changed=windows/mixtapes.ico");
        let version = |key: &str| std::env::var(key).unwrap_or_default();
        let macros = [
            format!("VERSION_MAJOR={}", version("CARGO_PKG_VERSION_MAJOR")),
            format!("VERSION_MINOR={}", version("CARGO_PKG_VERSION_MINOR")),
            format!("VERSION_PATCH={}", version("CARGO_PKG_VERSION_PATCH")),
        ];
        // windres runs from the crate root and llvm-rc from windows/, so the icon is found through the include dir.
        // Required: a Windows build that found no resource compiler would ship without its icon.
        let windows_dir = std::path::Path::new(&version("CARGO_MANIFEST_DIR")).join("windows");
        embed_resource::compile("windows/mixtapes.rc", embed_resource::ParamsMacrosAndIncludeDirs(&macros, [windows_dir])).manifest_required().unwrap();
    }
}
