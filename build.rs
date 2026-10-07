//! Compiles resources/resources.gresource.xml into the binary: the stylesheet and
//! the symbolic icons the app ships. Nothing is looked up on disk at run time,
//! so an installed binary and a source checkout behave the same.

// The PO reader the app's tests cover. Shared so the build and the tests agree on what a PO file means.
#[path = "src/i18n/po.rs"]
mod po;

/// Compile the translations named in po/LINGUAS into a Rust table, which src/i18n/mod.rs includes.
fn compile_translations() {
    use std::fmt::Write;
    println!("cargo:rerun-if-changed=po");
    println!("cargo:rerun-if-changed=src/i18n/po.rs");
    let linguas = std::fs::read_to_string("po/LINGUAS").unwrap_or_default();
    let mut out = String::from("pub static CATALOGS: &[RawCatalog] = &[\n");
    for lang in linguas.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#')) {
        let path = format!("po/{lang}.po");
        // A translation that does not parse is left out. The app still builds, in English for that language.
        let entries = match std::fs::read_to_string(&path).map_err(|err| err.to_string()).and_then(|text| po::parse(&text)) {
            Ok(entries) => entries,
            Err(err) => {
                println!("cargo:warning={path}: {err}");
                continue;
            }
        };
        let compiled = po::compile(&entries);
        for skipped in &compiled.skipped {
            println!("cargo:warning={path}: {skipped}");
        }
        let _ = writeln!(out, "    RawCatalog {{ lang: {lang:?}, plural_forms: {:?}, entries: &[", compiled.plural_forms);
        for (key, forms) in &compiled.entries {
            let _ = writeln!(out, "        ({key:?}, &{forms:?}),");
        }
        out.push_str("    ] },\n");
    }
    out.push_str("];\n");
    let dest = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("catalogs.rs");
    std::fs::write(dest, out).unwrap();
}

fn main() {
    compile_translations();

    // The icons stay in the repository's assets folder, which the README and the
    // Flatpak repo file link to, so there is one copy of each.
    println!("cargo:rerun-if-changed=assets/icons");
    println!("cargo:rerun-if-changed=resources");
    glib_build_tools::compile_resources(&["resources", "assets/icons"], "resources/resources.gresource.xml", "mixtapes.gresource");

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
