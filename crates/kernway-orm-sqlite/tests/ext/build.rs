fn main() {
    println!("cargo:rerun-if-changed=src/ext.c");
    cc::Build::new().file("src/ext.c").include("../../sqlite").compile("kw_test_ext_c");
}
