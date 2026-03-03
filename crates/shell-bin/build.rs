use std::env;

fn main() {
    let lib_dir =
        env::var("FERCUDA_LIB_DIR").unwrap_or_else(|_| "/home/daron/.local/lib".to_string());
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib_dir);
    println!("cargo:rerun-if-env-changed=FERCUDA_LIB_DIR");
}
