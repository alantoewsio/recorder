//! On Windows, copy vendored `libmp3lame.dll` next to the built `recorder-ui` so `mp3lame-encoder` can load it.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    #[cfg(windows)]
    {
        let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
        let repo_root = manifest_dir.join("../..");
        let dll_src = repo_root.join("third_party/lame/windows-x64/libmp3lame.dll");
        if dll_src.exists() {
            let profile = env::var("PROFILE").expect("PROFILE");
            let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
            let profile_dir = out_dir
                .ancestors()
                .find(|p| p.file_name().and_then(|n| n.to_str()) == Some(profile.as_str()))
                .map(PathBuf::from)
                .unwrap_or_else(|| repo_root.join("target").join(&profile));
            let _ = fs::create_dir_all(&profile_dir);
            let dll_dst = profile_dir.join("libmp3lame.dll");
            if let Err(e) = fs::copy(&dll_src, &dll_dst) {
                println!(
                    "cargo:warning=could not copy {} to {}: {}",
                    dll_src.display(),
                    dll_dst.display(),
                    e
                );
            }
            println!("cargo:rerun-if-changed={}", dll_src.display());
        }
    }
}
