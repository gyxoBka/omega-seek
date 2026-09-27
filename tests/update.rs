//! The binary can be replaced while a process runs it.

use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omega-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory can be created");
    dir
}

#[test]
fn a_running_binary_is_replaced_and_the_old_one_steps_aside() {
    let dir = scratch("update");
    let built = PathBuf::from(env!("CARGO_BIN_EXE_omega"));
    let name = built.file_name().unwrap().to_owned();
    let exe = dir.join(&name);
    std::fs::copy(&built, &exe).unwrap();
    let old = std::fs::read(&exe).unwrap();

    // A server that keeps the binary open, as an MCP session does.
    let mut held = std::process::Command::new(&exe)
        .args(["mcp", "--no-model", "--root"])
        .arg(&dir)
        .env("OMEGA_NO_DAEMON", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));

    // "New" bytes: the same binary with a byte appended, so a later run still works.
    let mut fresh = old.clone();
    fresh.push(b'\n');
    omega::update::replace(&exe, &fresh).unwrap();
    assert_eq!(std::fs::read(&exe).unwrap(), fresh, "the path holds the new binary");
    let listed = || -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let stepped_aside = listed().iter().filter(|name| name.contains(".old-")).count();
    if cfg!(windows) {
        assert_eq!(stepped_aside, 1, "the running binary stepped aside: {:?}", listed());
    } else {
        assert_eq!(stepped_aside, 0, "renamed over, nothing aside: {:?}", listed());
    }
    assert!(!listed().iter().any(|name| name.ends_with(".new")), "no .new left: {:?}", listed());

    // The session ends; the next update sweeps what it left.
    held.kill().unwrap();
    let _ = held.wait();
    std::thread::sleep(std::time::Duration::from_millis(300));
    omega::update::replace(&exe, &old).unwrap();
    assert_eq!(std::fs::read(&exe).unwrap(), old);
    assert!(!listed().iter().any(|name| name.contains(".old-")), "old ones swept: {:?}", listed());

    let _ = std::fs::remove_dir_all(&dir);
}
