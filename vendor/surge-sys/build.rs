use bindgen::callbacks::{DiscoveredItem, DiscoveredItemId};
use std::{
    collections::{HashSet, hash_map::DefaultHasher},
    env::{var, var_os},
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::{Command, Output},
};

use {bindgen, cmake, serde_json, shell_words};
// ^ here to easily check if they go unused.

macro_rules! realprint {
    ($($tokens:tt)*) => {
        println!("\x1b[1;32m[SRS-SYS] =>\x1b[0m {}", format!($($tokens)*));
    }
}
macro_rules! fakeprint {
    ($($tokens:tt)*) => {
        println!("\x1b[1;36m[SRS-SYS] =>\x1b[0m {}", format!($($tokens)*));
    }
}

macro_rules! linksearchlink {
    ($bpath:expr, $(($search:expr, $link:expr)),* $(,)?) => {
        $(
            println!("cargo:rustc-link-search=native={}", $bpath.clone() + "/build/" + $search);
            println!("cargo:rustc-link-lib=static={}", $link);
        )*
    }
}

const SURGE_REVISION: &str = "3c64680043bf8ef65cfcc6019e847c3f655c14fc";
const SURGE_REPOSITORY: &str = "https://github.com/surge-synthesizer/surge";

fn git_output(directory: &Path, arguments: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(arguments)
        .output()
        .unwrap_or_else(|error| panic!("failed to run system Git: {error}"))
}

fn run_git(directory: &Path, arguments: &[&str]) {
    let output = git_output(directory, arguments);
    if !output.status.success() {
        panic!(
            "git {} failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
}

fn checkout_surge_revision(directory: &Path) {
    let current = git_output(directory, &["rev-parse", "HEAD"]);
    if current.status.success() && String::from_utf8_lossy(&current.stdout).trim() == SURGE_REVISION
    {
        return;
    }
    run_git(
        directory,
        &["fetch", "--depth", "1", "origin", SURGE_REVISION],
    );
    run_git(
        directory,
        &["checkout", "--detach", "--force", SURGE_REVISION],
    );
}

fn synchronize_surge_submodules(directory: &Path) {
    run_git(directory, &["submodule", "sync", "--recursive"]);
    run_git(
        directory,
        &[
            "submodule",
            "update",
            "--init",
            "--recursive",
            "--depth",
            "1",
        ],
    );
}

fn pull_surge_from_clouds(dst: impl AsRef<Path>) {
    let dst = dst.as_ref();
    if dst.exists() {
        if git_output(dst, &["rev-parse", "--is-inside-work-tree"])
            .status
            .success()
        {
            checkout_surge_revision(dst);
            synchronize_surge_submodules(dst);
            realprint!("surge is down from the clouds. no action.");
            return;
        } else {
            realprint!("surge is down from the clouds, but it came down mangled.");
            assert_eq!(dst, Path::new(SDST_OT)); // just as safety.
            assert!(
                !std::fs::symlink_metadata(dst)
                    .expect("failed to inspect mangled Surge checkout")
                    .file_type()
                    .is_symlink(),
                "refusing to remove a symlinked Surge checkout"
            );
            std::fs::remove_dir_all(dst).expect("failed to remove mangled Surge checkout");
            realprint!("removed the mangled surge. poor thing.");
        }
    }

    realprint!("surge is in the sky. pulling surge from the clouds.");
    std::fs::create_dir_all(dst).expect("failed to create Surge checkout directory");
    run_git(dst, &["init"]);
    run_git(dst, &["remote", "add", "origin", SURGE_REPOSITORY]);
    fakeprint!("...");
    checkout_surge_revision(dst);
    fakeprint!("\x1B[AOK.                                                           ");

    // sorry for writing this one. m(._.)m
    realprint!("the pulled surge is stable, but we need to fill its innards with joy.");
    synchronize_surge_submodules(dst);
    realprint!("surge is ready.");
}

fn build_surge_from_ground(src: impl AsRef<Path>) -> PathBuf {
    let src = src.as_ref();
    let cmake_lists = src.join("CMakeLists.txt");
    let cmake = std::fs::read_to_string(&cmake_lists).expect("failed to read Surge CMakeLists.txt");
    let rust_lua_disable = r#"if(SURGE_BUILD_RS)
    message(STATUS "Lua is being disabled due to temporary incompatibility with Rust bindings.")
    set(SURGE_SKIP_LUA TRUE)
endif()

"#;
    if cmake.contains(rust_lua_disable) {
        std::fs::write(&cmake_lists, cmake.replace(rust_lua_disable, ""))
            .expect("failed to enable Surge Formula for Rust bindings");
    }
    // Cargo gives Clippy, tests, and normal builds distinct OUT_DIR fingerprints.
    // Keep the expensive native build at the profile level so those commands share it.
    let profile_dir = PathBuf::from(var_os("OUT_DIR").expect("Cargo did not set OUT_DIR"))
        .ancestors()
        .nth(3)
        .expect("OUT_DIR does not have Cargo's expected layout")
        .to_path_buf();
    let mut source_hasher = DefaultHasher::new();
    var("CARGO_MANIFEST_DIR")
        .expect("Cargo did not set CARGO_MANIFEST_DIR")
        .hash(&mut source_hasher);
    SURGE_REVISION.hash(&mut source_hasher);
    cmake::Config::new(src)
        .out_dir(profile_dir.join(format!("surge-native-{:016x}", source_hasher.finish())))
        .define("SURGE_SKIP_JUCE_FOR_RACK", "ON")
        .define("SURGE_SKIP_VST3", "ON")
        .define("SURGE_SKIP_ALSA", "ON")
        .define("SURGE_SKIP_STANDALONE", "ON")
        .define("SURGE_SKIP_LUA", "OFF")
        .define("CMAKE_EXPORT_COMPILE_COMMANDS", "ON")
        .define("ENABLE_LTO", "OFF")
        .build()
}

const SDST_OT: &str = "sbmod/surge/"; // i kind of forgot what this acronym stood for.
const SDST_IT: &str = "../../../"; // the surge in surge/src/surge-rs/surge-rs.

#[derive(Debug)]
struct BindReporter;

impl bindgen::callbacks::ParseCallbacks for BindReporter {
    fn header_file(&self, filename: &str) {
        fakeprint!("{: <12}{}", "HEADER:", filename);
    }
    fn include_file(&self, filename: &str) {
        fakeprint!("{: <12}{}", "INCLUDE:", filename);
    }
    fn read_env_var(&self, key: &str) {
        fakeprint!("{: <12}{}", "ENV:", key);
    }
    fn new_item_found(&self, id: DiscoveredItemId, item: DiscoveredItem) {
        //let nfnon = "...".to_string();  // "name for no original name."
        let get_id = |x: DiscoveredItemId| {
            format!("{:?}", x)
                .trim_start_matches("DiscoveredItemId(")
                .trim_end_matches(")")
                .parse::<usize>()
                .unwrap()
        };

        let packed = match item {
            //DiscoveredItem::Struct { original_name, final_name }    => (original_name.unwrap_or("???".to_string()), final_name),
            //DiscoveredItem::Union { original_name, final_name }     => (original_name.unwrap_or("???".to_string()), final_name),
            DiscoveredItem::Alias {
                alias_name,
                alias_for,
            } => Some((
                format!("ALIAS OF {:0>6}", get_id(alias_for)).to_string(),
                alias_name,
            )),
            //DiscoveredItem::Enum { final_name }                     => (nfnon, final_name),
            //DiscoveredItem::Function { final_name }                 => (nfnon, final_name),
            DiscoveredItem::Method { final_name, parent } => Some((
                format!("CHILD OF {:0>6}", get_id(parent)).to_string(),
                final_name,
            )),
            _ => None,
        };
        if let Some((from, to)) = packed {
            fakeprint!("ID {:0>6} => {} -> {: >65}]", get_id(id), from, to);
        }
    }
    /*fn item_name(&self, item_info: ItemInfo) -> Option<String> {
        let kind = match item_info.kind {
            bindgen::callbacks::ItemKind::Module    => "MOD",
            bindgen::callbacks::ItemKind::Type      => "TYP",
            bindgen::callbacks::ItemKind::Function  => "FUN",
            bindgen::callbacks::ItemKind::Var       => "VAR",
            _                                       => "???",
        };
        fakeprint!("{}:\t{}", kind, item_info.name);
        None
    }*/
    /*fn generated_name_override(&self, item_info: ItemInfo) -> Option<String> {
        self.item_name(item_info);
        None
    }*/
}

// okay. let's use some comments to keep our minds fresh.
fn main() {
    // rerun this entire script if any of these files change.
    println!("cargo:rerun-if-changed=cpp/plumber.h"); // the plumber.
    println!("cargo:rerun-if-changed=cpp/plumber.cpp"); // fixes leaks in bindgen.
    println!("cargo:rerun-if-changed=wrapper.h");

    // set build and source paths for surge, depending on build mode.
    // TODO: allow custom directory or keep tree mode?
    let sdst_ot = SDST_OT.to_string();
    let sdst_it = SDST_IT.to_string();
    let (spath, bpath) = if var("CARGO_FEATURE_IN_SURGE_TREE").is_ok() {
        realprint!("feature \"in-surge-tree\" enabled. using parent directories.");
        (sdst_it.clone(), sdst_it)
    } else {
        realprint!("feature \"in-surge-tree\" disabled. pulling surge.");
        pull_surge_from_clouds(&sdst_ot);
        let bdst = build_surge_from_ground(&sdst_ot);
        (sdst_ot, bdst.to_string_lossy().to_string()) // why do i have to do this dance?...
    };

    linksearchlink!(
        bpath,
        ("src/common", "surge-common"),
        ("src/lua", "surge-lua-src"),
        ("libs/luajitlib/LuaJIT/src/LuaJIT/src", "luajit"),
        ("libs/zstd/build/cmake/lib", "zstd"),
        ("libs/sqlite-3.23.3", "sqlite"),
        ("libs/oddsound-mts", "oddsound-mts"),
        (
            "libs/fmt",
            if var("OPT_LEVEL").unwrap() != "0" {
                "fmt"
            } else {
                "fmtd"
            }
        ), // why.
        ("libs/pffft", "pffft"),
        ("libs/eurorack", "eurorack"),
        ("libs/binn", "binn"),
        ("libs/airwindows", "airwindows"),
        ("libs/sst/sst-plugininfra", "sst-plugininfra"),
        ("libs/sst/sst-plugininfra/libs/strnatcmp", "strnatcmp"),
        ("libs/sst/sst-plugininfra/libs/tinyxml", "tinyxml"),
    );
    realprint!("peeking into (and exporting) surge's build flags.");
    let comcom = bpath.clone() + "/build/compile_commands.json"; // "compile commands". comcom.
    let json = std::fs::read_to_string(&comcom).expect("failed to read comcom!");
    let coms: serde_json::Value = serde_json::from_str(&json).expect("failed to parse comcom!");

    // get and use all the include paths from the configure.
    let mut unique = HashSet::new();
    for entry in coms.as_array().unwrap() {
        if let Some(clist) = entry.get("command") {
            shell_words::split(clist.as_str().unwrap())
                .unwrap()
                .into_iter()
                .filter(|x| x.starts_with("-I") || x.starts_with("-D"))
                .for_each(|x| {
                    unique.insert(x);
                })
        }
    }
    let mut unique: Vec<_> = unique.into_iter().collect(); // not sorting *will* crash the build.
    unique.sort(); // like, at some point. hard to tell.
    let prepath = PathBuf::from(var("CARGO_MANIFEST_DIR").unwrap())
        .display()
        .to_string()
        + "/";
    println!(
        "cargo:bflags={}",
        unique.join(",") + ",-I" + &prepath + &spath
    );

    realprint!("searching for what the glue should bind.");
    let mut bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg("-I".to_owned() + &spath) // crazy you gotta do this owned stuff.
        .clang_arg("-x")
        .clang_arg("c++")
        .clang_arg("-std=c++20")
        .clang_arg("-fno-char8_t") // fix for compilation. present in cmake, surely.
        .layout_tests(false) // fix for unnecessary checks that overflow (good job).
        .opaque_type("std::.*") // fix for stl type exports (obvious).
        .blocklist_item("fmt::.*") // fix for formatting lib exports (can't be represented).
        .blocklist_item("FP_INT__.*") // fix for double definition (math.h likely).
        .blocklist_item("size_type") // fix for something with a looping equivalent (somehow).
        .blocklist_item("const_pointer") // fix for multiple definitions (of a basic term).
        .blocklist_item("rep") // fix for multiple definitions (of whatever that is).
        .blocklist_item("int_type") // fix for multiple definitions (of a second basic term).
        .blocklist_item("char_type") // fix for multiple definitions (of a third basic term).
        .blocklist_item("iterator") // fix for multiple definitions (of a complex term).
        .blocklist_item("FE_.*") // fix for various double definitions (FE?).
        .blocklist_item("FP_.*") // fix for various double definitions (FE counterpart?).
        .blocklist_item("__gnu_.*") // fix for proprietary data (somewhat).
        .blocklist_function("SurgeSynthesizer::idForParameter")
        .allowlist_item("Surge.*") // fix for everything else (the nuclear option).
        .allowlist_item(".*idFor.*") // fix for functions i need (unexported).
        .allowlist_item(".*Storage.*") // fix for surge storage (most stuff).
        .allowlist_item(".*State.*") // fix for surge storage (other stuff).
        .parse_callbacks(Box::new(BindReporter));

    realprint!("setting up the bindgen plumber.");
    let mut bbuild = cc::Build::new();
    bbuild
        .warnings(false)
        .cpp(true)
        .std("c++20")
        .include(spath.clone())
        .flag("-fno-char8_t") // read PRE-ahead. this has to go here too...
        .file("cpp/plumber.cpp"); // (that means read up. this block moved.)

    realprint!("applying surge powder to the glue and pipes.");
    for flag in unique {
        fakeprint!("new flag: {}", flag);
        bbuild.flag(&flag);
        bindings = bindings.clone().clang_arg(&flag); // is this not, like, bad or something?
    }

    realprint!("generating bindings. please hold so i can make the glue.");
    let storehere = PathBuf::from(var("OUT_DIR").unwrap()).join("bindings.rs");
    bindings
        .generate()
        .expect("unable to generate surge bindings")
        .write_to_file(storehere)
        .expect("couldn't write bindings.");

    realprint!("pipes are being assembled. please hold.");
    let out = bbuild.try_compile("plumber");
    if let Err(e) = out {
        panic!("pipes burst while building. -> \"{}\"", e);
    } // TODO: do this with other errors (the arrow thing).
    println!("cargo:rustc-link-lib=static=plumber");

    realprint!("all done!");
}
