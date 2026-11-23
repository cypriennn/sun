fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        eprintln!("sun only supports Linux targets");
        std::process::exit(1);
    }
}
