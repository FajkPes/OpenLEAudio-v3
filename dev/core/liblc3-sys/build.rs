use std::path::PathBuf;

// The twelve translation units listed by liblc3's own src/makefile.mk. Kept as
// an explicit list rather than a directory glob: a glob would silently pick up
// the NEON and Arm intrinsic variants that the upstream makefile deliberately
// leaves out of a portable build.
const SOURCES: &[&str] = &[
    "attdet.c", "bits.c", "bwdet.c", "energy.c", "lc3.c", "ltpf.c", "mdct.c",
    "plc.c", "sns.c", "spec.c", "tables.c", "tns.c",
];

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("liblc3-sys always sits one level below dev/core")
        .join("vendor/liblc3");

    let src = root.join("src");
    let include = root.join("include");

    assert!(
        include.join("lc3.h").is_file(),
        "vendored liblc3 is missing at {}. Run:\n  \
         git clone --depth 1 https://github.com/google/liblc3.git dev/core/vendor/liblc3",
        root.display()
    );

    let mut build = cc::Build::new();
    build.include(&include).include(&src);

    for file in SOURCES {
        build.file(src.join(file));
    }

    // liblc3 is written to C11. MSVC defaults to an older dialect and rejects
    // the mixed declarations and _Static_assert the codec relies on.
    if build.get_compiler().is_like_msvc() {
        build.flag("/std:c11");
        // The upstream headers mark the public API __declspec(dllexport) on
        // _WIN32 unconditionally. That is correct for the DLL build and merely
        // noisy for a static one, so the resulting export warnings are silenced
        // rather than the vendored source being patched.
        build.flag("/wd4197");
    } else {
        build.flag("-std=c11");
    }

    build.compile("lc3");

    println!("cargo:rerun-if-changed={}", src.display());
    println!("cargo:rerun-if-changed={}", include.display());
}
