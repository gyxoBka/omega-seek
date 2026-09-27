use omega::access::Access;
use omega::roots::clean;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn workspace(name: &str) -> PathBuf {
    let ws = std::env::temp_dir().join(format!("omega-access-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ws);
    for dir in ["app/.git/worktrees/fix", "app/src", "app-fix/src", "lib/.git", "lib/src", "other/.git"] {
        std::fs::create_dir_all(ws.join(dir)).unwrap();
    }
    let ws = clean(&ws.canonicalize().unwrap());
    std::fs::create_dir_all(home_of(&ws)).unwrap();
    std::fs::write(ws.join("app/src/main.rs"), "fn start_app() {\n    todo!()\n}\n").unwrap();
    std::fs::write(ws.join("app-fix/src/main.rs"), "fn start_fix() {\n    todo!()\n}\n").unwrap();
    std::fs::write(ws.join("lib/src/parse.rs"), "fn parse_lib() {\n    todo!()\n}\n").unwrap();
    std::fs::write(ws.join("app-fix/.git"), format!("gitdir: {}\n", ws.join("app/.git/worktrees/fix").display())).unwrap();
    std::fs::write(ws.join("app/.git/worktrees/fix/commondir"), "../..\n").unwrap();
    std::fs::write(ws.join("app/.git/worktrees/fix/gitdir"), format!("{}\n", ws.join("app-fix/.git").display())).unwrap();
    std::fs::write(ws.join("app/.git/worktrees/fix/HEAD"), "ref: refs/heads/fix\n").unwrap();
    ws
}

fn home_of(ws: &Path) -> PathBuf {
    ws.with_file_name(format!("{}-home", ws.file_name().unwrap().to_string_lossy()))
}

fn settings(ws: &Path) -> PathBuf {
    ws.join("config/omega/config.json")
}

#[test]
fn a_repository_and_its_worktrees_are_readable_and_nothing_else() {
    let ws = workspace("own");
    let access = Access::load_from(&settings(&ws)).unwrap();
    let (app, fix) = (ws.join("app"), ws.join("app-fix"));
    assert!(access.allows(&app, &app.join("src")));
    assert!(access.allows(&app, &fix), "the main checkout reads its worktree");
    assert!(access.allows(&fix, &app), "a worktree reads its main checkout");
    assert_eq!(omega::roots::repository(&fix), app);
    for outside in [ws.join("lib"), ws.clone(), ws.join("other")] {
        assert!(!access.allows(&app, &outside), "{}", outside.display());
        assert!(!access.allows(&fix, &outside), "{}", outside.display());
    }
    let refusal = access.refusal(&app, &ws.join("lib"));
    assert!(refusal.contains("omega access add") && refusal.contains(&omega::roots::shown(&app)), "{refusal}");
    let _ = std::fs::remove_dir_all(home_of(&ws));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn access_given_to_a_repository_is_its_worktrees_and_no_one_else() {
    let ws = workspace("given");
    let file = settings(&ws);
    let (app, fix, lib, other) = (ws.join("app"), ws.join("app-fix"), ws.join("lib"), ws.join("other"));
    let mut access = Access::load_from(&file).unwrap();
    assert!(access.grant(&app, &lib));
    assert!(!access.grant(&app, &lib), "given twice is given once");
    access.save().unwrap();

    let access = Access::load_from(&file).unwrap();
    assert!(access.allows(&app, &lib) && access.allows(&app, &lib.join("src")));
    assert!(access.allows(&fix, &lib), "a worktree has what its repository has");
    assert!(!access.allows(&lib, &app), "access is not mutual");
    assert!(!access.allows(&other, &lib), "nor shared with other repositories");
    assert!(!access.allows(&app, &other));

    let mut access = access;
    access.grant(&app, &ws);
    assert!(access.allows(&app, &other) && access.allows(&app, &ws));
    assert!(access.revoke(&app, &ws) && !access.allows(&app, &other));
    assert!(access.forget(&app) && !access.allows(&app, &lib));
    let _ = std::fs::remove_dir_all(home_of(&ws));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn settings_keep_what_is_not_ours_and_refuse_what_is_not_json() {
    let ws = workspace("settings");
    let file = settings(&ws);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, r#"{"daemon": false}"#).unwrap();
    let mut access = Access::load_from(&file).unwrap();
    access.grant(&ws.join("app"), &ws.join("lib"));
    access.save().unwrap();
    let written: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(written["daemon"], serde_json::json!(false));
    assert!(written["access"].is_object());

    std::fs::write(&file, "not json").unwrap();
    assert!(Access::load_from(&file).unwrap_err().contains("not a JSON object"));
    let _ = std::fs::remove_dir_all(home_of(&ws));
    let _ = std::fs::remove_dir_all(&ws);
}

fn omega(ws: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_omega"));
    for (name, dir) in [
        ("APPDATA", "config"),
        ("XDG_CONFIG_HOME", "config"),
        ("LOCALAPPDATA", "cache"),
        ("XDG_CACHE_HOME", "cache"),
    ] {
        command.env(name, ws.join(dir));
    }
    command.env("USERPROFILE", home_of(ws)).env("HOME", home_of(ws)).env("OMEGA_NO_DAEMON", "1");
    command
}

fn stores(ws: &Path) -> usize {
    std::fs::read_dir(ws.join("cache/omega/index"))
        .map(|entries| entries.flatten().filter(|entry| entry.path().extension().is_some_and(|ext| ext == "idx")).count())
        .unwrap_or(0)
}

#[test]
fn a_directory_outside_is_refused_before_it_is_indexed_until_access_is_given() {
    let ws = workspace("server");
    let mut server = omega(&ws)
        .args(["mcp", "--no-model", "--root"])
        .arg(ws.join("app"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = server.stdin.take().unwrap();
    let mut output = BufReader::new(server.stdout.take().unwrap());
    let mut id = 0;
    let mut call = |tool: &str, arguments: serde_json::Value| -> serde_json::Value {
        id += 1;
        let message = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": tool, "arguments": arguments}});
        writeln!(input, "{message}").unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str::<serde_json::Value>(&line).unwrap()["result"].clone()
    };
    let text = |result: &serde_json::Value| result["content"][0]["text"].as_str().unwrap_or_default().to_owned();

    let own = call("search", serde_json::json!({"query": "start_app"}));
    assert!(text(&own).contains("src/main.rs"), "{}", text(&own));
    let worktree = call("search", serde_json::json!({"query": "start_fix", "root": "../app-fix"}));
    assert!(text(&worktree).contains("src/main.rs"), "a worktree needs no access: {}", text(&worktree));
    let before = stores(&ws);

    let refused = call("search", serde_json::json!({"query": "parse_lib", "root": "../lib"}));
    assert_eq!(refused["isError"], serde_json::json!(true));
    assert!(text(&refused).contains("omega access add"), "{}", text(&refused));
    let dotted = call("search", serde_json::json!({"query": "parse_lib", "root": "src/../../lib"}));
    assert_eq!(dotted["isError"], serde_json::json!(true), "{dotted}");
    let absolute = ws.join("lib/src/parse.rs").to_string_lossy().into_owned();
    let refused = call("outline", serde_json::json!({"path": absolute}));
    assert!(text(&refused).contains("omega access add"), "{}", text(&refused));
    let listed = call("outline", serde_json::json!({"path": "", "root": ".."}));
    assert!(listed["isError"].is_null() && text(&listed).contains("Readable now"), "{listed}");
    assert_eq!(stores(&ws), before, "nothing refused was indexed");

    let given = omega(&ws).args(["access", "add", "../lib"]).current_dir(ws.join("app-fix")).output().unwrap();
    assert!(given.status.success(), "{}", String::from_utf8_lossy(&given.stderr));
    let found = call("search", serde_json::json!({"query": "parse_lib", "root": "../lib"}));
    assert!(text(&found).contains("parse.rs"), "{}", text(&found));

    drop(input);
    let _ = server.wait();
    let _ = std::fs::remove_dir_all(home_of(&ws));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn access_is_given_from_inside_a_repository_and_listed() {
    let ws = workspace("command");
    let run = |args: &[&str], dir: &Path| {
        let output = omega(&ws).args(args).current_dir(dir).output().unwrap();
        (output.status.success(), String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr))
    };
    let (ok, said) = run(&["access", "add", "../../lib"], &ws.join("app/src"));
    assert!(ok && said.contains("may now read"), "{said}");
    let (_, said) = run(&["access", "list"], &ws.join("app-fix"));
    assert!(said.contains(&omega::roots::shown(&ws.join("lib"))), "a worktree lists its repository's access: {said}");
    let (_, said) = run(&["access", "add", "../app-fix"], &ws.join("app"));
    assert!(said.contains("already"), "a worktree needs no access: {said}");
    let (ok, said) = run(&["access", "add", "../lib"], &home_of(&ws));
    assert!(!ok && said.contains("not a repository"), "{said}");
    let (_, said) = run(&["access", "list", "--all"], &home_of(&ws));
    assert!(said.contains(&omega::roots::shown(&ws.join("app"))), "{said}");
    let (ok, said) = run(&["access", "remove", "../lib"], &ws.join("app"));
    assert!(ok && said.contains("no longer"), "{said}");
    let _ = std::fs::remove_dir_all(home_of(&ws));
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn stores_not_opened_for_a_month_are_pruned_and_nothing_else() {
    let dir = std::env::temp_dir().join(format!("omega-prune-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 24 * 3600);
    let mut head = b"OMEGAIDX".to_vec();
    head.extend_from_slice(&omega::store::VERSION.to_le_bytes());
    head.extend_from_slice(&[0; 12]);
    for (name, aged) in [("old.idx", true), ("old.bin", false), ("fresh.idx", false), ("left.idx.1.tmp", true), ("notes.txt", true)] {
        let path = dir.join(name);
        std::fs::write(&path, &head).unwrap();
        if aged {
            std::fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
        }
    }
    let (files, _) = omega::store::prune(&dir, Some(std::time::Duration::from_secs(30 * 24 * 3600)));
    assert_eq!(files, 3, "the old store, the store of an earlier release whatever its age, what a crash left aside");
    let mut left: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    assert_eq!(left, ["fresh.idx", "notes.txt"]);

    let store = dir.join("fresh.idx");
    std::fs::File::options().write(true).open(&store).unwrap().set_modified(old).unwrap();
    omega::store::touch(&store);
    let age = std::fs::metadata(&store).unwrap().modified().unwrap().elapsed().unwrap_or_default();
    assert!(age < std::time::Duration::from_secs(60));
    let _ = std::fs::remove_dir_all(&dir);
}
