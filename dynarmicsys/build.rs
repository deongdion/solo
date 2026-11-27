//! Builds the pinned dynarmic submodule with CMake and the C shim with cc.
//!
//! Boost headers are required by dynarmic. Point `DYNARMIC_BOOST_INCLUDE` at a Boost source tree
//! (the directory that contains `boost/`).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const LIBS: [&str; 5] = ["dynarmic", "mcl", "fmt", "Zydis", "Zycore"];

/// Fixes to the pinned dynarmic, applied to the checkout before it is built: (file, original,
/// replacement). Applying is idempotent; a file that has neither text fails the build.
const PATCHES: [(&str, &str, &str); 1] = [(
    // Dead code elimination treated memory reads as free of side effects, so a load whose value
    // is unused (`ldrb wzr, [x0]`, the usual memory probe) was removed and never faulted.
    "src/dynarmic/ir/opt/dead_code_elimination_pass.cpp",
    "if (!inst.HasUses() && !inst.MayHaveSideEffects()) {",
    "if (!inst.HasUses() && !inst.MayHaveSideEffects() && !inst.IsMemoryRead()) {",
)];

fn apply_patches(source: &Path) {
    for (file, original, replacement) in PATCHES {
        let path = source.join(file);
        println!("cargo:rerun-if-changed={}", path.display());
        let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        if text.contains(replacement) {
            continue;
        }
        assert!(text.contains(original), "{}: patch does not apply (dynarmic changed?)", path.display());
        fs::write(&path, text.replacen(original, replacement, 1)).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
}

fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let source = root.join("dynarmic");
    let shim = root.join("shim");
    println!("cargo:rerun-if-changed=shim");
    println!("cargo:rerun-if-env-changed=DYNARMIC_BOOST_INCLUDE");

    apply_patches(&source);

    let boost = env::var("DYNARMIC_BOOST_INCLUDE")
        .expect("DYNARMIC_BOOST_INCLUDE must point at a Boost source tree (the directory containing boost/)");

    // The cmake crate replaces CMAKE_<LANG>_FLAGS_RELEASE with cc's flags, which carry no optimization
    // level for MSVC. Without this the JIT itself is built unoptimized.
    let optimize = if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") { "/O2 /DNDEBUG" } else { "-O2 -DNDEBUG" };
    let out = cmake::Config::new(&source)
        .profile("Release")
        .cflag(optimize)
        .cxxflag(optimize)
        .define("DYNARMIC_TESTS", "OFF")
        .define("DYNARMIC_WARNINGS_AS_ERRORS", "OFF")
        .define("DYNARMIC_USE_BUNDLED_EXTERNALS", "ON")
        .define("DYNARMIC_FRONTENDS", "A32;A64")
        .define("Boost_INCLUDE_DIR", &boost)
        .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
        .define("CMAKE_MSVC_RUNTIME_LIBRARY", "MultiThreadedDLL")
        .define("CMAKE_POSITION_INDEPENDENT_CODE", "ON")
        .build_target("dynarmic")
        .build();

    for name in LIBS {
        let dir = find_library(&out, name).unwrap_or_else(|| panic!("static library {name} not found under {}", out.display()));
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=static={name}");
    }

    cc::Build::new()
        .cpp(true)
        .std("c++20")
        .file(shim.join("shim.cpp"))
        .include(&shim)
        .include(source.join("src"))
        .include(source.join("externals/mcl/include"))
        .include(source.join("externals/fmt/include"))
        .include(&boost)
        .define("NOMINMAX", None)
        .flag_if_supported("/EHsc")
        .flag_if_supported("/permissive-")
        .warnings(false)
        .compile("dynshim");
}

fn find_library(dir: &Path, name: &str) -> Option<PathBuf> {
    let candidates = [format!("{name}.lib"), format!("lib{name}.a")];
    let entries = fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_library(&path, name) {
                return Some(found);
            }
        } else if let Some(file) = path.file_name().and_then(|f| f.to_str())
            && candidates.iter().any(|c| c == file) {
                return Some(dir.to_path_buf());
            }
    }
    None
}
