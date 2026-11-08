//! The prebuilt ISPC objects in `intel_tex_2` reference the C++ exception
//! personality routine, so any final binary needs the C++ runtime linked.
fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    match target_os.as_str() {
        "linux" => println!("cargo:rustc-link-lib=stdc++"),
        "macos" => println!("cargo:rustc-link-lib=c++"),
        _ => {}
    }
}
