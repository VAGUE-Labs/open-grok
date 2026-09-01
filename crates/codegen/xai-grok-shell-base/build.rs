use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let repository_root = manifest_dir.join("../../..");
    let version_path = repository_root.join("OPEN_GROK_VERSION");
    println!("cargo:rerun-if-changed={}", version_path.display());

    let bundled_path = env::var_os("OUT_DIR")
        .map(PathBuf::from)
        .expect("OUT_DIR must be set")
        .join("bundled_changelog.md");

    let release_notes = fs::read_to_string(&version_path)
        .ok()
        .map(|version| {
            repository_root
                .join("docs/releases")
                .join(format!("v{}.md", version.trim()))
        })
        .filter(|path| {
            println!("cargo:rerun-if-changed={}", path.display());
            path.is_file()
        })
        .and_then(|path| fs::read_to_string(path).ok())
        .unwrap_or_default();

    fs::write(bundled_path, release_notes).expect("write bundled changelog");
}
