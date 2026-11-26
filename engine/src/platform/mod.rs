mod window;

pub use window::Platform;

use std::path::{Path, PathBuf};

/// Where a game's asset folders (`assets/`, `profiles/`, ...) live at runtime.
///
/// * Web build: `/`, the root of the in-memory file system that
///   `package_web.sh` preloads each asset folder into.
/// * Native: next to the executable when an `assets/` folder is there (a
///   packaged build); otherwise, in debug builds only, `dev_manifest_dir`
///   (pass `env!("CARGO_MANIFEST_DIR")`) so `cargo run` reads the sources.
pub fn asset_root(dev_manifest_dir: &str) -> PathBuf {
    if cfg!(target_os = "emscripten") {
        return PathBuf::from("/");
    }
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    if exe_dir.join("assets").is_dir() {
        return exe_dir;
    }
    if cfg!(debug_assertions) {
        let dev = PathBuf::from(dev_manifest_dir);
        if dev.join("assets").is_dir() {
            return dev;
        }
    }
    let _ = dev_manifest_dir;
    exe_dir
}

#[cfg(test)]
mod tests {
    #[test]
    fn debug_builds_fall_back_to_the_manifest_dir() {
        let dir = std::env::temp_dir().join(format!("dust_asset_root_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        let root = super::asset_root(dir.to_str().unwrap());
        if cfg!(debug_assertions) {
            assert_eq!(root, dir);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
