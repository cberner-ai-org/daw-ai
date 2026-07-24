use std::{fs, path::Path};

const SURGE_RS_REVISION: &str = "7bfeafc76d1c57860a177e9e076bed7ec764009a";
const SURGE_XT_REVISION: &str = "3c64680043bf8ef65cfcc6019e847c3f655c14fc";

fn read(root: &Path, path: &str) -> String {
    fs::read_to_string(root.join(path))
        .unwrap_or_else(|error| panic!("could not read vendored boundary file {path}: {error}"))
}

#[test]
fn vendored_surge_boundary_keeps_pins_and_patched_api() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = read(root, "Cargo.toml");
    assert!(manifest.contains(&format!("rev = \"{SURGE_RS_REVISION}\"")));
    for crate_name in ["surge-bridge", "surge-rs", "surge-sys"] {
        assert!(manifest.contains(&format!(
            "{crate_name} = {{ path = \"vendor/{crate_name}\" }}"
        )));
        assert!(read(root, &format!("vendor/{crate_name}/PATCHES.md")).contains(SURGE_RS_REVISION));
    }

    let sys_patches = read(root, "vendor/surge-sys/PATCHES.md");
    assert!(sys_patches.contains(SURGE_XT_REVISION));

    let binding = read(root, "vendor/surge-rs/src/glue/synthesizer.rs");
    assert!(binding.contains("pub fn set_input_buffer"));
    assert!(binding.contains("pub fn pull_buffer"));

    let service = read(root, "deploy/daw-ai.service");
    assert!(service.contains("DynamicUser=yes"));
    assert!(!service.contains("User=daw-ai"));
    assert!(!service.contains("Group=daw-ai"));
}
