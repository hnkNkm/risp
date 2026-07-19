fn main() {
    cc::Build::new()
        .file("runtime/risp_rt.c")
        .include("runtime")
        .opt_level(2)
        .warnings(true)
        .compile("risp_rt");
    println!("cargo:rerun-if-changed=runtime/risp_rt.c");
    println!("cargo:rerun-if-changed=runtime/risp_rt.h");
}