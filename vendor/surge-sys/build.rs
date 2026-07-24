use std::{collections::HashSet, env::var, path::{Path, PathBuf}};
use bindgen::callbacks::{DiscoveredItem, DiscoveredItemId};
use git2::{build::CheckoutBuilder, Oid};

use {bindgen, serde_json, shell_words, cmake, git2};
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

fn pct_callback(c: &mut u32, p: git2::Progress<'_>) -> bool {
    *c += 1;
    if *c == 10 {
        *c = 0;
        let percentage =
            p.received_objects() as f32
            / p.total_objects() as f32
            * 100.0;

        fakeprint!(
            "\x1B[APULL...  {:6.2}%  ({: >5}/{: <5}); {} bytes.",
            percentage,
            p.received_objects(),
            p.total_objects(),
            p.received_bytes(),
        );
    }

    true
}

fn chk_callback(p: Option<&Path>, c: usize, t: usize) {
    let mut p = if let Some(path) = p {
        path.to_string_lossy().to_string()
    } else {
        "???".to_string()
    };
    if p.chars().count() > 25 {
        p = "...".to_string() + &p.chars()
            .rev().take(22).collect::<String>().chars()
            .rev().collect::<String>();
    }

    let percentage = c as f32 / t as f32 * 100.0;

    fakeprint!(
        "\x1B[ACHECK... {:6.2}%  ({: >5}/{: <5}); @ {: >25}.",
        percentage,
        c,
        t,
        p,
    );
}

fn sm_update_rec(repo: &git2::Repository) {
    let sms = repo.submodules().expect("the surge's insides are devoid of instructions...");
    for mut sm in sms {
        let name = sm.name().unwrap_or("???");
        realprint!("injecting \"{}\" into the surge.", name);

        sm.init(false).expect("joy initialization failed.");
        let mut counter = 0;
        let mut uopts = git2::SubmoduleUpdateOptions::new();
        let mut fopts = git2::FetchOptions::new();
        let mut callbacks = git2::RemoteCallbacks::new();
        let mut checkout = CheckoutBuilder::new();
        callbacks.transfer_progress(|p| pct_callback(&mut counter, p));
        checkout.progress(|p, c, t| chk_callback(p, c, t));
        fopts.remote_callbacks(callbacks).prune(git2::FetchPrune::On);
        uopts.fetch(fopts).checkout(checkout);

        fakeprint!("...");
        sm.update(true, Some(&mut uopts)).expect("failed to introduce into the surge.");
        fakeprint!("\x1B[AOK.                                                           ");

        if let Ok(repo) = sm.open() {
            if repo.submodules().unwrap().len() > 0 {
                fakeprint!("found extra goodies to insert.");
                sm_update_rec(&repo);
            }
        }
    }
}

const SURGE_REVISION: &str = "3c64680043bf8ef65cfcc6019e847c3f655c14fc";

fn checkout_surge_revision(repo: &git2::Repository) {
    let revision = Oid::from_str(SURGE_REVISION).expect("invalid pinned Surge XT revision");
    if repo.head().ok().and_then(|head| head.target()) == Some(revision) {
        return;
    }

    let mut fetch = git2::FetchOptions::new();
    fetch.depth(1);
    repo.find_remote("origin")
        .expect("Surge XT checkout has no origin")
        .fetch(&[SURGE_REVISION], Some(&mut fetch), None)
        .expect("failed to fetch the pinned Surge XT revision");
    let object = repo
        .find_object(revision, None)
        .expect("pinned Surge XT revision was not fetched");
    repo.checkout_tree(
        &object,
        Some(CheckoutBuilder::new().force()),
    )
    .expect("failed to check out the pinned Surge XT revision");
    repo.set_head_detached(revision)
        .expect("failed to detach the pinned Surge XT revision");
}

fn pull_surge_from_clouds(dst: impl AsRef<Path>) {
    let dst = dst.as_ref();
    if dst.exists() {
        if let Ok(repo) = git2::Repository::open(dst) {
            checkout_surge_revision(&repo);
            realprint!("surge is down from the clouds. no action.");
            return;
        } else {
            realprint!("surge is down from the clouds, but it came down mangled.");
            assert_eq!(dst.to_str().unwrap(), "sbmod/surge/");  // just as safety.
            std::fs::remove_dir_all(dst).unwrap();
            realprint!("removed the mangled surge. poor thing.");
        }
    }

    realprint!("surge is in the sky. pulling surge from the clouds.");
    let mut counter = 0;
    let mut callbacks = git2::RemoteCallbacks::new();
    let mut checkout = CheckoutBuilder::new();
    callbacks.transfer_progress(|p| pct_callback(&mut counter, p));
    checkout.progress(|p, c, t| chk_callback(p, c, t));

    let mut fopts = git2::FetchOptions::new();
    fopts.depth(1).remote_callbacks(callbacks).prune(git2::FetchPrune::On);

    fakeprint!("...");
    git2::build::RepoBuilder::new()
        .fetch_options(fopts)
        .with_checkout(checkout)
        .clone("https://github.com/surge-synthesizer/surge", dst)
        .expect("the sun came up, so we were unable to pull surge from the clouds.");
    fakeprint!("\x1B[AOK.                                                           ");

    // sorry for writing this one. m(._.)m
    realprint!("the pulled surge is stable, but we need to fill its innards with joy.");
    let repo = git2::Repository::open(dst).expect("somehow couldn't crack open the surge.");
    checkout_surge_revision(&repo);
    sm_update_rec(&repo);
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
    cmake::Config::new(src)
        .define("SURGE_SKIP_JUCE_FOR_RACK", "ON")
        .define("SURGE_SKIP_VST3", "ON")
        .define("SURGE_SKIP_ALSA", "ON")
        .define("SURGE_SKIP_STANDALONE", "ON")
        .define("SURGE_SKIP_LUA", "OFF")
        .define("CMAKE_EXPORT_COMPILE_COMMANDS", "ON")
        .define("ENABLE_LTO", "OFF")
        .build()
}

const SDST_OT: &str = "sbmod/surge/";   // i kind of forgot what this acronym stood for.
const SDST_IT: &str = "../../../";      // the surge in surge/src/surge-rs/surge-rs.

#[derive(Debug)]
struct BindReporter;

impl bindgen::callbacks::ParseCallbacks for BindReporter {
    fn header_file(&self, filename: &str) { fakeprint!("{: <12}{}", "HEADER:", filename); }
    fn include_file(&self, filename: &str) { fakeprint!("{: <12}{}", "INCLUDE:", filename); }
    fn read_env_var(&self, key: &str) { fakeprint!("{: <12}{}", "ENV:", key); }
    fn new_item_found(&self, id: DiscoveredItemId, item: DiscoveredItem) {
        //let nfnon = "...".to_string();  // "name for no original name."
        let get_id = |x: DiscoveredItemId|
            format!("{:?}", x).trim_start_matches("DiscoveredItemId(").trim_end_matches(")").parse::<usize>().unwrap();

        let packed = match item {
            //DiscoveredItem::Struct { original_name, final_name }    => (original_name.unwrap_or("???".to_string()), final_name),
            //DiscoveredItem::Union { original_name, final_name }     => (original_name.unwrap_or("???".to_string()), final_name),
            DiscoveredItem::Alias { alias_name, alias_for }         => Some((format!("ALIAS OF {:0>6}", get_id(alias_for)).to_string(), alias_name)),
            //DiscoveredItem::Enum { final_name }                     => (nfnon, final_name),
            //DiscoveredItem::Function { final_name }                 => (nfnon, final_name),
            DiscoveredItem::Method { final_name, parent }           => Some((format!("CHILD OF {:0>6}", get_id(parent)).to_string(), final_name)),
            _                                                       => None,
        };
        if let Some((from, to)) = packed { fakeprint!("ID {:0>6} => {} -> {: >65}]", get_id(id), from, to); }
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
    println!("cargo:rerun-if-changed=cpp/plumber.h");           // the plumber.
    println!("cargo:rerun-if-changed=cpp/plumber.cpp");         // fixes leaks in bindgen.
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
        (sdst_ot, bdst.to_string_lossy().to_string())   // why do i have to do this dance?...
    };

    linksearchlink!(bpath,
        ("src/common",                              "surge-common"),
        ("src/lua",                                 "surge-lua-src"),
        ("libs/luajitlib/LuaJIT/src/LuaJIT/src",    "luajit"),
        ("libs/zstd/build/cmake/lib",               "zstd"),
        ("libs/sqlite-3.23.3",                      "sqlite"),
        ("libs/oddsound-mts",                       "oddsound-mts"),
        ("libs/fmt",                                if var("OPT_LEVEL").unwrap() != "0" { "fmt" } else { "fmtd" }), // why.
        ("libs/pffft",                              "pffft"),
        ("libs/eurorack",                           "eurorack"),
        ("libs/binn",                               "binn"),
        ("libs/airwindows",                         "airwindows"),
        ("libs/sst/sst-plugininfra",                "sst-plugininfra"),
        ("libs/sst/sst-plugininfra/libs/strnatcmp", "strnatcmp"),
        ("libs/sst/sst-plugininfra/libs/tinyxml",   "tinyxml"),
    );
    realprint!("peeking into (and exporting) surge's build flags.");
    let comcom = bpath.clone() + "/build/compile_commands.json";    // "compile commands". comcom.
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
                .for_each(|x| { unique.insert(x); })
        }
    }
    let mut unique: Vec<_> = unique.into_iter().collect();  // not sorting *will* crash the build.
    unique.sort();                                          // like, at some point. hard to tell.
    let prepath = PathBuf::from(var("CARGO_MANIFEST_DIR").unwrap()).display().to_string() + "/";
    println!("cargo:bflags={}", unique.join(",") + ",-I" + &prepath + &spath);

    realprint!("searching for what the glue should bind.");
    let mut bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg("-I".to_owned() + &spath)    // crazy you gotta do this owned stuff.
        .clang_arg("-x")
        .clang_arg("c++")
        .clang_arg("-std=c++20")
	.clang_arg("-fno-char8_t")          // fix for compilation. present in cmake, surely.
        .layout_tests(false)                // fix for unnecessary checks that overflow (good job).
        .opaque_type("std::.*")             // fix for stl type exports (obvious).
        .blocklist_item("fmt::.*")          // fix for formatting lib exports (can't be represented).
        .blocklist_item("FP_INT__.*")       // fix for double definition (math.h likely).
        .blocklist_item("size_type")        // fix for something with a looping equivalent (somehow).
        .blocklist_item("const_pointer")    // fix for multiple definitions (of a basic term).
        .blocklist_item("rep")              // fix for multiple definitions (of whatever that is).
        .blocklist_item("int_type")         // fix for multiple definitions (of a second basic term).
        .blocklist_item("char_type")        // fix for multiple definitions (of a third basic term).
        .blocklist_item("iterator")         // fix for multiple definitions (of a complex term).
        .blocklist_item("FE_.*")            // fix for various double definitions (FE?).
        .blocklist_item("FP_.*")            // fix for various double definitions (FE counterpart?).
        .blocklist_item("__gnu_.*")         // fix for proprietary data (somewhat).
        .allowlist_item("Surge.*")          // fix for everything else (the nuclear option).
        .allowlist_item(".*idFor.*")        // fix for functions i need (unexported).
        .allowlist_item(".*Storage.*")      // fix for surge storage (most stuff).
        .allowlist_item(".*State.*")        // fix for surge storage (other stuff).
        .parse_callbacks(Box::new(BindReporter));

    realprint!("setting up the bindgen plumber.");
    let mut bbuild = cc::Build::new();
    bbuild
        .warnings(false)
        .cpp(true)
        .std("c++20")
        .include(spath.clone())
        .flag("-fno-char8_t")               // read PRE-ahead. this has to go here too...
        .file("cpp/plumber.cpp");           // (that means read up. this block moved.)

    realprint!("applying surge powder to the glue and pipes.");
    for flag in unique {
        fakeprint!("new flag: {}", flag);
        bbuild.flag(&flag);
        bindings = bindings.clone().clang_arg(&flag);   // is this not, like, bad or something?
    }

    realprint!("generating bindings. please hold so i can make the glue.");
    let storehere = PathBuf::from(var("OUT_DIR").unwrap()).join("bindings.rs");
    bindings
        .generate().expect("unable to generate surge bindings")
        .write_to_file(storehere).expect("couldn't write bindings.");

    realprint!("pipes are being assembled. please hold.");
    let out = bbuild.try_compile("plumber");
    if let Err(e) = out { panic!("pipes burst while building. -> \"{}\"", e); } // TODO: do this with other errors (the arrow thing).
    println!("cargo:rustc-link-lib=static=plumber");


    realprint!("all done!");
}
