fn main() {
    let version = std::env::var("POROS_VERSION").unwrap_or_else(|_| "dev".to_string());
    if !version.is_empty() {
        println!("cargo:rustc-env=POROS_VERSION={version}");
    }
}
