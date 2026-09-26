//! Compiles resources/resources.gresource.xml into the binary: the stylesheet and
//! the symbolic icons the app ships. Nothing is looked up on disk at run time,
//! so an installed binary and a source checkout behave the same.

fn main() {
    // The icons stay in the repository's assets folder, which the README and the
    // Flatpak repo file link to, so there is one copy of each.
    println!("cargo:rerun-if-changed=assets/icons");
    println!("cargo:rerun-if-changed=resources");
    glib_build_tools::compile_resources(&["resources", "assets/icons"], "resources/resources.gresource.xml", "mixtapes.gresource");
}
