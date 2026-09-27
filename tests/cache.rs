use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omega-cache-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["home", "config", "cache/omega/index", "run"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    dir
}

fn omega(dir: &Path, args: &[&str]) -> (bool, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_omega"));
    for (name, sub) in [
        ("APPDATA", "config"),
        ("XDG_CONFIG_HOME", "config"),
        ("LOCALAPPDATA", "cache"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_RUNTIME_DIR", "run"),
        ("USERPROFILE", "home"),
        ("HOME", "home"),
    ] {
        command.env(name, dir.join(sub));
    }
    let output = command.args(args).output().unwrap();
    (output.status.success(), String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr))
}

fn store(dir: &Path, name: &str) {
    let mut bytes = b"OMEGAIDX".to_vec();
    bytes.extend_from_slice(&omega::store::VERSION.to_le_bytes());
    bytes.extend_from_slice(&[0; 40]);
    std::fs::write(dir.join("cache/omega/index").join(name), bytes).unwrap();
}

#[test]
fn the_cache_is_shown_cleaned_cleared_and_its_cleaning_set() {
    let dir = scratch("commands");
    store(&dir, "a.idx");
    store(&dir, "b.idx");
    std::fs::write(dir.join("cache/omega/index/old.bin"), vec![0u8; 1000]).unwrap();

    let (ok, said) = omega(&dir, &["cache"]);
    assert!(ok && said.contains("2 stores") && said.contains("1 outdated") && said.contains("30 days"), "{said}");

    let (ok, said) = omega(&dir, &["cache", "clean"]);
    assert!(ok && said.contains("removed 1 file"), "{said}");
    assert!(!dir.join("cache/omega/index/old.bin").exists() && dir.join("cache/omega/index/a.idx").exists());

    let (ok, said) = omega(&dir, &["cache", "auto", "7"]);
    assert!(ok && said.contains("7 days"), "{said}");
    assert!(omega(&dir, &["cache"]).1.contains("7 days"));
    let (ok, said) = omega(&dir, &["cache", "auto", "off"]);
    assert!(ok && said.contains("off"), "{said}");
    assert!(!omega(&dir, &["cache", "auto", "soon"]).0);
    let settings = std::fs::read_to_string(dir.join("config/omega/config.json")).unwrap();
    assert!(settings.contains("clean_after_days"), "{settings}");

    let (ok, said) = omega(&dir, &["cache", "clear"]);
    assert!(ok && said.contains("removed 2 files"), "{said}");
    assert!(!dir.join("cache/omega/index").exists());
    let _ = std::fs::remove_dir_all(&dir);
}
