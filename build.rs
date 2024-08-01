fn main() {
    cc::Build::new()
        .file("src/native/defines.c")
        .compile("defines");
    println!(
        "cargo:rustc-link-search=native={}",
        std::env::var("OUT_DIR").unwrap()
    );
    println!("cargo:rustc-link-lib=static=defines");
}
