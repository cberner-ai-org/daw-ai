use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::{env, fs};

pub(crate) const FACTORY_PREFIX: &str = "Factory/";
const HEADLESS_UNSAFE_PRESETS: &[&str] = &[];

#[cfg(test)]
pub(crate) fn headless_unsafe_presets() -> &'static [&'static str] {
    HEADLESS_UNSAFE_PRESETS
}

pub(crate) fn headless_render_error(id: &str) -> Option<String> {
    HEADLESS_UNSAFE_PRESETS.contains(&id).then(|| {
        format!(
            "Surge XT preset {id} is disabled because it crashes the headless audio renderer; choose another preset"
        )
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Preset {
    pub(crate) id: String,
    pub(crate) category: String,
    pub(crate) name: String,
    pub(crate) path: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct PresetFolder {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) preset_count: usize,
}

#[derive(Debug)]
pub(crate) struct PresetLevel<'a> {
    pub(crate) path: String,
    pub(crate) parent: Option<String>,
    pub(crate) folders: Vec<PresetFolder>,
    pub(crate) presets: Vec<&'a Preset>,
}

pub(crate) fn catalog() -> Vec<Preset> {
    let Some(root) = factory_root() else {
        return Vec::new();
    };
    catalog_for_root(&root).as_ref().clone()
}

pub(crate) fn render_safe_catalog() -> Vec<Preset> {
    catalog()
        .into_iter()
        .filter(|preset| headless_render_error(&preset.id).is_none())
        .collect()
}

fn catalog_for_root(root: &Path) -> Arc<Vec<Preset>> {
    static CATALOGS: OnceLock<Mutex<HashMap<PathBuf, Arc<Vec<Preset>>>>> = OnceLock::new();
    let catalogs = CATALOGS.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(catalog) = catalogs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(root)
        .cloned()
    {
        return catalog;
    }

    let mut presets = Vec::new();
    collect_presets(root, root, &mut presets);
    presets.sort_by(|left, right| {
        left.category
            .cmp(&right.category)
            .then_with(|| left.name.cmp(&right.name))
    });
    let catalog = Arc::new(presets);
    catalogs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(root.to_owned())
        .or_insert_with(|| Arc::clone(&catalog))
        .clone()
}

pub(crate) fn find(id: &str) -> Option<Preset> {
    if !is_factory_id(id) {
        return None;
    }
    let root = factory_root()?;
    catalog_for_root(&root)
        .iter()
        .find(|preset| preset.id == id)
        .cloned()
}

pub(crate) fn browse<'a>(presets: &'a [Preset], path: &str) -> Option<PresetLevel<'a>> {
    let path = path.trim_matches('/');
    if path != "Factory" && !path.starts_with(FACTORY_PREFIX) {
        return None;
    }
    let relative = path.strip_prefix(FACTORY_PREFIX).unwrap_or("");
    let prefix = if relative.is_empty() {
        String::new()
    } else {
        format!("{relative}/")
    };
    let mut folders = BTreeMap::<String, usize>::new();
    let mut direct = Vec::new();
    let mut exists = relative.is_empty();
    for preset in presets {
        if preset.category == relative {
            direct.push(preset);
            exists = true;
            continue;
        }
        let Some(remainder) = preset.category.strip_prefix(&prefix) else {
            continue;
        };
        exists = true;
        let child = remainder.split('/').next().unwrap_or(remainder);
        *folders.entry(child.to_owned()).or_default() += 1;
    }
    exists.then(|| PresetLevel {
        path: path.to_owned(),
        parent: path.rsplit_once('/').map(|(parent, _)| parent.to_owned()),
        folders: folders
            .into_iter()
            .map(|(name, preset_count)| PresetFolder {
                path: format!("{path}/{name}"),
                name,
                preset_count,
            })
            .collect(),
        presets: direct,
    })
}

pub(crate) fn is_factory_id(value: &str) -> bool {
    let Some(relative) = value.strip_prefix(FACTORY_PREFIX) else {
        return false;
    };
    !relative.is_empty()
        && !relative.starts_with('/')
        && !relative.ends_with('/')
        && relative
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn factory_root() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = env::var_os("DAW_AI_SURGE_PRESET_DIR") {
        candidates.push(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("SURGE_DATA_HOME") {
        let path = PathBuf::from(path);
        candidates.push(path.join("patches_factory"));
        candidates.push(path.join("resources/data/patches_factory"));
    }
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("vendor/surge-sys/sbmod/surge/resources/data/patches_factory"),
    );
    candidates.into_iter().find(|path| path.is_dir())
}

fn collect_presets(root: &Path, directory: &Path, presets: &mut Vec<Preset>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_presets(root, &path, presets);
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("fxp") {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        let category = relative
            .parent()
            .map(|parent| parent.to_string_lossy().replace('\\', "/"))
            .filter(|category| !category.is_empty())
            .unwrap_or_else(|| "Uncategorized".to_owned());
        if category == "FX" && name == "Aggero" {
            continue;
        }
        presets.push(Preset {
            id: format!("{FACTORY_PREFIX}{category}/{name}"),
            category,
            name: name.to_owned(),
            path,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_root_catalog_is_indexed_once() {
        let root = std::env::temp_dir().join(format!(
            "daw-ai-preset-cache-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let pads = root.join("Pads");
        fs::create_dir_all(&pads).expect("preset fixture directory");
        fs::write(pads.join("Cached.fxp"), b"fixture").expect("preset fixture");

        let first = catalog_for_root(&root);
        let second = catalog_for_root(&root);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id, "Factory/Pads/Cached");

        fs::remove_dir_all(root).expect("remove preset fixture");
    }

    #[test]
    fn factory_ids_are_stable_and_safe() {
        assert!(is_factory_id("Factory/Pads/Flux Capacitor"));
        assert!(!is_factory_id("Factory/../secret"));
        assert!(!is_factory_id("Factory/Pads/"));
    }

    #[test]
    fn checked_out_factory_catalog_is_discoverable() {
        let presets = catalog();
        assert!(presets.len() > 100);
        assert!(
            presets
                .iter()
                .any(|preset| preset.id == "Factory/Pads/Flux Capacitor")
        );
        assert!(
            presets
                .iter()
                .all(|preset| preset.id != "Factory/FX/Aggero")
        );
        let safe = render_safe_catalog();
        assert_eq!(safe.len(), presets.len() - HEADLESS_UNSAFE_PRESETS.len());
        assert!(
            safe.iter()
                .all(|preset| !HEADLESS_UNSAFE_PRESETS.contains(&preset.id.as_str()))
        );
    }

    #[test]
    fn catalog_is_browsed_one_level_at_a_time() {
        let presets = catalog();
        let root = browse(&presets, "Factory").expect("factory root");
        assert!(root.presets.is_empty());
        assert!(root.folders.iter().any(|folder| {
            folder.name == "Pads" && folder.path == "Factory/Pads" && folder.preset_count > 10
        }));
        let pads = browse(&presets, "Factory/Pads").expect("pads");
        assert_eq!(pads.parent.as_deref(), Some("Factory"));
        assert!(
            pads.presets
                .iter()
                .any(|preset| preset.name == "Flux Capacitor")
        );
        assert!(browse(&presets, "Factory/Not Here").is_none());
    }
}
