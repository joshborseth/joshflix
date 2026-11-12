//! Compiles the Slint UI and stamps the Android version code into the binary.

fn main() {
    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles);
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI failed to compile");

    // android/version.toml: `version_code = N`. Keeps About and the updater honest.
    let version_file = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../android/version.toml");
    println!("cargo:rerun-if-changed={}", version_file.display());
    if let Ok(text) = std::fs::read_to_string(&version_file)
        && let Some(code) = text
            .lines()
            .filter_map(|l| l.split_once('='))
            .find(|(k, _)| k.trim() == "version_code")
            .and_then(|(_, v)| v.trim().trim_matches('"').parse::<u64>().ok())
    {
        println!("cargo:rustc-env=JOSHFLIX_VERSION_CODE={code}");
    }
    println!("cargo:rerun-if-env-changed=JOSHFLIX_SERVER_URL");
}
