use omega::install::Request;
use omega::install::remove::{Found, Part, ticked, warnings, without_directory, without_marked_lines};
use std::path::{Path, PathBuf};
use std::process::Command;

fn everything() -> Found {
    Found {
        agents: vec!["Claude Code".to_owned(), "Codex".to_owned()],
        stores: Some((PathBuf::from("/c/omega/index"), 5 << 20, 3)),
        model: Some((PathBuf::from("/d/omega/models"), 32 << 20)),
        settings: Some((PathBuf::from("/s/omega/config.json"), 1)),
        path_entry: Some("/bin/omega".to_owned()),
        binary: Some(PathBuf::from("/bin/omega/omega")),
        foreign_binary: None,
    }
}

#[test]
fn everything_there_is_ticked_unless_told_otherwise() {
    let found = everything();
    assert_eq!(ticked(&found, &Request::default(), false), Part::ALL);
    assert_eq!(ticked(&found, &Request::default(), true), [Part::Integrations, Part::PathEntry, Part::Binary]);
    let agents = Request { agents: Some(vec!["claude".to_owned()]), ..Request::default() };
    assert_eq!(ticked(&found, &agents, false), [Part::Integrations]);
    let partial = Found { model: None, settings: None, path_entry: None, ..everything() };
    assert_eq!(partial.present(), [Part::Integrations, Part::Stores, Part::Binary]);
    assert!(Found::default().present().is_empty());
}

#[test]
fn a_choice_that_leaves_something_broken_is_said_to() {
    let found = everything();
    let said = warnings(&found, &[Part::Binary], &["Claude Code".to_owned()]);
    assert!(said.iter().any(|warning| warning.contains("Claude Code will still start")), "{said:?}");
    assert!(said.iter().any(|warning| warning.contains("PATH entry")), "{said:?}");
    assert!(warnings(&found, &[Part::Model], &[]).iter().any(|warning| warning.contains("lexical")));
    assert!(warnings(&found, &Part::ALL, &[]).is_empty());
}

#[test]
fn the_path_loses_our_directory_and_profiles_our_lines() {
    assert_eq!(
        without_directory(r"C:\a;C:\Users\me\AppData\Local\Programs\omega\;C:\b", r"C:\Users\me\AppData\Local\Programs\omega").as_deref(),
        Some(r"C:\a;C:\b")
    );
    assert_eq!(without_directory(r"C:\a;C:\b", r"C:\omega"), None);
    let profile = "alias ll='ls -l'\n\nexport PATH=\"/home/me/.local/bin:$PATH\" # added by omega\n";
    assert_eq!(without_marked_lines(profile).as_deref(), Some("alias ll='ls -l'\n\n"));
    assert_eq!(without_marked_lines("nothing of ours\n"), None);
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omega-uninstall-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for sub in ["home", "config/omega", "cache/omega/index", "cache/omega/models/m", "data"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    std::fs::write(dir.join("config/omega/config.json"), r#"{"access": {"/a": ["/b"]}}"#).unwrap();
    std::fs::write(dir.join("cache/omega/index/0123.idx"), vec![0u8; 2048]).unwrap();
    std::fs::write(dir.join("cache/omega/models/m/model.safetensors"), vec![0u8; 4096]).unwrap();
    std::fs::write(dir.join("cache/omega/daemon-abc.pid"), "{}").unwrap();
    std::fs::write(dir.join("home/neighbour.txt"), "not ours").unwrap();
    dir
}

fn omega(dir: &Path, args: &[&str]) -> (bool, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_omega"));
    for (name, sub) in [
        ("OMEGA_HOME", "home"),
        ("APPDATA", "config"),
        ("XDG_CONFIG_HOME", "config"),
        ("LOCALAPPDATA", "cache"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_DATA_HOME", "cache"),
        ("XDG_RUNTIME_DIR", "data"),
        ("USERPROFILE", "home"),
        ("HOME", "home"),
    ] {
        command.env(name, dir.join(sub));
    }
    let output = command.args(args).output().unwrap();
    (output.status.success(), String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr))
}

fn claude_has_omega(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("home/.claude.json")).is_ok_and(|text| text.contains("\"omega\""))
}

#[test]
fn uninstall_takes_out_what_was_chosen_and_nothing_else() {
    let dir = scratch("full");
    std::fs::create_dir_all(dir.join("home/.claude")).unwrap();
    let (ok, said) = omega(&dir, &["install", "--agents", "claude", "--yes"]);
    assert!(ok && claude_has_omega(&dir), "{said}");

    let (ok, said) = omega(&dir, &["uninstall"]);
    assert!(!ok && said.contains("--yes"), "no terminal, no questions: {said}");

    let (ok, said) = omega(&dir, &["uninstall", "--yes", "--dry-run"]);
    assert!(ok && said.contains("Dry run") && said.contains("Index caches") && said.contains("Claude Code"), "{said}");
    assert!(claude_has_omega(&dir) && dir.join("cache/omega/index/0123.idx").is_file());

    let (ok, said) = omega(&dir, &["uninstall", "--yes", "--agents", "claude", "--integrations", "instructions"]);
    assert!(ok && claude_has_omega(&dir), "only the instructions went: {said}");
    assert!(dir.join("cache/omega/models/m/model.safetensors").is_file());

    let (ok, said) = omega(&dir, &["uninstall", "--yes", "--keep-data"]);
    assert!(ok && !claude_has_omega(&dir), "{said}");
    assert!(dir.join("cache/omega/index/0123.idx").is_file() && dir.join("config/omega/config.json").is_file());
    assert!(said.contains("cargo clean"), "a binary under target/ is not ours to remove: {said}");

    let (ok, said) = omega(&dir, &["uninstall", "--yes"]);
    assert!(ok, "{said}");
    for gone in ["cache/omega/index", "cache/omega/models", "config/omega/config.json", "cache/omega/daemon-abc.pid"] {
        assert!(!dir.join(gone).exists(), "{gone} is still there: {said}");
    }
    assert!(dir.join("home/neighbour.txt").is_file(), "what is not ours stays");
    assert!(Path::new(env!("CARGO_BIN_EXE_omega")).is_file());

    let (ok, said) = omega(&dir, &["uninstall", "--yes"]);
    assert!(ok && said.contains("Nothing of omega's is left"), "{said}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(windows)]
#[test]
fn a_running_binary_removes_itself_once_it_has_ended() {
    let dir = scratch("self");
    let installed = dir.join("programs/omega");
    std::fs::create_dir_all(&installed).unwrap();
    let exe = installed.join("omega.exe");
    std::fs::copy(env!("CARGO_BIN_EXE_omega"), &exe).unwrap();
    std::fs::write(installed.join("omega.exe.old-1"), b"an old one").unwrap();
    let mut command = Command::new(&exe);
    for (name, sub) in [("OMEGA_HOME", "home"), ("APPDATA", "config"), ("LOCALAPPDATA", "cache"), ("USERPROFILE", "home")] {
        command.env(name, dir.join(sub));
    }
    let output = command.args(["uninstall", "--yes"]).output().unwrap();
    let said = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(output.status.success() && said.contains("binary"), "{said}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while installed.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert!(!installed.exists(), "the binary and its directory are gone: {said}");
    let _ = std::fs::remove_dir_all(&dir);
}
