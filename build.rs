fn main() {
    println!("cargo:rerun-if-changed=protocols/river-window-management-v1.xml");
    println!("cargo:rerun-if-changed=protocols/river-xkb-bindings-v1.xml");
    println!("cargo:rustc-link-search=native=/usr/local/lib");
}
