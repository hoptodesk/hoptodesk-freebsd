fn main() {
    hbb_common::gen_version();
    println!("cargo:rerun-if-changed=build.rs");
}
