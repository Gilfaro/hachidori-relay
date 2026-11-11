// SPDX-License-Identifier: GPL-3.0-or-later
//! Builds the native Hoshidicts engine for host mode (`native/`, through cmake) and
//! links it, and packs the language packs it ships (`language-packs.bin`). Without
//! the `host` feature there is nothing to build.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    #[cfg(feature = "host")]
    native::build();
    #[cfg(feature = "host")]
    packs::build();
}

/// The language packs host mode ships, the ones Hachidori's
/// `scripts/sync-languages.mjs` copies into the extension: every directory of
/// `third_party/hoshidicts/languages` but `_template`, with the JSON files at its
/// top level (`language.json` and the transforms it names). One deflated stream of
/// `(code, file name, contents)` records, each field a little-endian u32 length
/// and its bytes; `src/host/engine/languages.rs` reads it.
#[cfg(feature = "host")]
#[allow(clippy::expect_used, reason = "a build script fails by panicking")]
mod packs {
    use std::env;
    use std::fs;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    pub(super) fn build() {
        let root = Path::new("third_party/hoshidicts/languages");
        println!("cargo:rerun-if-changed={}", root.display());
        let mut codes: Vec<PathBuf> = fs::read_dir(root)
            .expect("the Hoshidicts language packs")
            .map(|entry| entry.expect("a language pack directory").path())
            .filter(|path| path.is_dir() && path.file_name().and_then(|name| name.to_str()).is_some_and(|name| !name.starts_with('_')))
            .collect();
        codes.sort();
        let mut records = Vec::new();
        let mut field = |bytes: &[u8]| {
            records.extend_from_slice(&u32::try_from(bytes.len()).expect("a pack file under 4 GiB").to_le_bytes());
            records.extend_from_slice(bytes);
        };
        for dir in &codes {
            let code = dir.file_name().and_then(|name| name.to_str()).expect("a UTF-8 language code");
            let mut files: Vec<PathBuf> = fs::read_dir(dir)
                .expect("a language pack")
                .map(|entry| entry.expect("a language pack file").path())
                .filter(|path| path.is_file() && path.extension().is_some_and(|extension| extension == "json"))
                .collect();
            files.sort();
            for file in files {
                println!("cargo:rerun-if-changed={}", file.display());
                field(code.as_bytes());
                field(file.file_name().and_then(|name| name.to_str()).expect("a UTF-8 file name").as_bytes());
                field(&fs::read(&file).expect("a language pack file"));
            }
        }
        let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("language-packs.bin");
        let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(&records).expect("deflating the language packs");
        fs::write(out, encoder.finish().expect("deflating the language packs")).expect("writing the language packs");
    }
}

#[cfg(feature = "host")]
mod native {
    use std::env;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    pub(super) fn build() {
        println!("cargo:rerun-if-changed=native");
        println!("cargo:rerun-if-changed=third_party/hoshidicts/src");
        println!("cargo:rerun-if-changed=third_party/hoshidicts/include");
        assert!(
            Path::new("third_party/hoshidicts/external/glaze/include").is_dir(),
            "host mode needs the Hoshidicts sources: run `git submodule update --init --recursive`"
        );
        let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
        let mut config = cmake::Config::new("native");
        // Debug C++ is far too slow to look anything up with; the engine is always optimised.
        config.profile("Release").build_target("install");
        if target_os == "windows" {
            // Legacy-charset MDict stylesheets need iconv, which Windows lacks; such a
            // sheet is left out, as in an extension build without it.
            config.define("CMAKE_DISABLE_FIND_PACKAGE_Iconv", "ON");
        }
        if target_env == "msvc" {
            // Rust links the release C runtime, even into debug builds.
            config.define("CMAKE_MSVC_RUNTIME_LIBRARY", "MultiThreadedDLL");
        }
        let out = config.build();
        println!("cargo:rustc-link-search=native={}", out.join("lib").display());
        for lib in ["hachidori_native", "hoshidicts", "lzokay", "gumbo"] {
            println!("cargo:rustc-link-lib=static={lib}");
        }
        // The vendored C libraries name their archives differently per toolchain.
        let lib_dir = out.join("lib");
        for candidates in [["zstd_static", "zstd"], ["deflatestatic", "deflate"], ["utf8proc_static", "utf8proc"]] {
            let found = candidates
                .iter()
                .find(|name| lib_dir.join(format!("{name}.lib")).exists() || lib_dir.join(format!("lib{name}.a")).exists());
            let name = found.unwrap_or_else(|| panic!("no {} library in {}", candidates[1], lib_dir.display()));
            println!("cargo:rustc-link-lib=static={name}");
        }
        match (target_os.as_str(), target_env.as_str()) {
            (_, "msvc") => {}
            ("macos" | "ios", _) => println!("cargo:rustc-link-lib=c++"),
            // MinGW: the C++ runtime is linked statically, so the binary needs no MSYS2 DLLs.
            ("windows", _) => {
                for archive in ["libstdc++.a", "libwinpthread.a"] {
                    if let Some(dir) = compiler_file(archive).and_then(|file| file.parent().map(Path::to_path_buf)) {
                        println!("cargo:rustc-link-search=native={}", dir.display());
                    }
                }
                println!("cargo:rustc-link-lib=static:+verbatim=libstdc++.a");
                println!("cargo:rustc-link-lib=static:+verbatim=libwinpthread.a");
            }
            _ => println!("cargo:rustc-link-lib=stdc++"),
        }
    }

    /// Where the C++ compiler cmake used keeps one of its own files.
    fn compiler_file(name: &str) -> Option<PathBuf> {
        let compiler = env::var("CXX").unwrap_or_else(|_| "g++".to_owned());
        let output = Command::new(compiler).arg(format!("-print-file-name={name}")).output().ok()?;
        let path = PathBuf::from(String::from_utf8(output.stdout).ok()?.trim());
        path.is_absolute().then_some(path)
    }
}
