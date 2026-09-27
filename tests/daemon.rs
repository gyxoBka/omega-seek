use omega::roots::clean;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

fn workspace(name: &str) -> PathBuf {
    let ws = std::env::temp_dir().join(format!("omega-daemon-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ws);
    for dir in ["app/.git/worktrees/fix", "app/src", "app-fix/src", "home", "config", "cache", "run"] {
        std::fs::create_dir_all(ws.join(dir)).unwrap();
    }
    let ws = clean(&ws.canonicalize().unwrap());
    std::fs::write(ws.join("app/src/main.rs"), "fn start_app() {\n    todo!()\n}\n").unwrap();
    std::fs::write(ws.join("app-fix/src/main.rs"), "fn start_fix() {\n    todo!()\n}\n").unwrap();
    std::fs::write(ws.join("app-fix/.git"), format!("gitdir: {}\n", ws.join("app/.git/worktrees/fix").display())).unwrap();
    std::fs::write(ws.join("app/.git/worktrees/fix/commondir"), "../..\n").unwrap();
    std::fs::write(ws.join("app/.git/worktrees/fix/gitdir"), format!("{}\n", ws.join("app-fix/.git").display())).unwrap();
    ws
}

fn omega(ws: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_omega"));
    for (name, dir) in [
        ("APPDATA", "config"),
        ("XDG_CONFIG_HOME", "config"),
        ("LOCALAPPDATA", "cache"),
        ("XDG_CACHE_HOME", "cache"),
        ("XDG_RUNTIME_DIR", "run"),
        ("USERPROFILE", "home"),
        ("HOME", "home"),
    ] {
        command.env(name, ws.join(dir));
    }
    command.env("OMEGA_DAEMON_IDLE_SECS", "60").env_remove("OMEGA_NO_DAEMON");
    command
}

fn run(ws: &Path, args: &[&str]) -> (bool, String) {
    let output = omega(ws).args(args).arg("--no-model").current_dir(ws.join("home")).output().unwrap();
    (output.status.success(), String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr))
}

struct Session {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    id: u64,
}

impl Session {
    fn start(ws: &Path, home: &Path) -> Self {
        let mut child = omega(ws)
            .args(["mcp", "--no-model", "--root"])
            .arg(home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self { child, input, output, id: 0 }
    }

    fn search(&mut self, arguments: Value) -> String {
        self.id += 1;
        let message = json!({"jsonrpc": "2.0", "id": self.id, "method": "tools/call", "params": {"name": "search", "arguments": arguments}});
        writeln!(self.input, "{message}").unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        reply["result"]["content"][0]["text"].as_str().unwrap_or_default().to_owned()
    }

    fn end(mut self) {
        drop(self.input);
        let _ = self.child.wait();
    }
}

fn pid(status: &str) -> Option<u32> {
    let at = status.find("(pid ")? + 5;
    status[at..].split(')').next()?.parse().ok()
}

fn pid_files(ws: &Path) -> usize {
    std::fs::read_dir(ws.join("cache/omega"))
        .map(|entries| entries.flatten().filter(|entry| entry.file_name().to_string_lossy().ends_with(".pid")).count())
        .unwrap_or(0)
}

fn records(ws: &Path) -> usize {
    std::fs::read_dir(ws.join("cache/omega/index"))
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "idx"))
        .map(|entry| std::fs::read(entry.path()).unwrap().windows(8).filter(|window| window == b"OMEGAIDX").count())
        .sum()
}

fn finish(ws: &Path) {
    let _ = run(ws, &["daemon", "stop", "--all"]);
    let _ = std::fs::remove_dir_all(ws);
}

#[test]
fn sessions_share_one_daemon_and_a_worktree_is_answered_from_itself() {
    let ws = workspace("shared");
    let mut main = Session::start(&ws, &ws.join("app"));
    let mut fix = Session::start(&ws, &ws.join("app-fix"));
    assert!(main.search(json!({"query": "start_app"})).contains("src/main.rs"));
    assert!(fix.search(json!({"query": "start_fix"})).contains("src/main.rs"));
    let from_fix = fix.search(json!({"query": "start_app"}));
    assert!(!from_fix.contains("src/main.rs"), "the worktree was answered from the main checkout: {from_fix}");
    let (ok, status) = run(&ws, &["daemon", "status"]);
    assert!(ok, "{status}");
    assert!(status.contains("2 sessions"), "{status}");
    assert!(status.contains(&omega::roots::shown(&ws.join("app"))) && status.contains(&omega::roots::shown(&ws.join("app-fix"))), "{status}");
    assert_eq!(pid_files(&ws), 1);
    main.end();
    fix.end();
    finish(&ws);
}

#[test]
fn a_daemon_killed_or_stopped_is_started_again_by_the_next_call() {
    let ws = workspace("again");
    let mut session = Session::start(&ws, &ws.join("app"));
    assert!(session.search(json!({"query": "start_app"})).contains("src/main.rs"));
    let first = pid(&run(&ws, &["daemon", "status"]).1).expect("a pid");
    let _ = if cfg!(windows) {
        Command::new("taskkill").args(["/F", "/PID", &first.to_string()]).output()
    } else {
        Command::new("kill").args(["-9", &first.to_string()]).output()
    };
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(session.search(json!({"query": "start_app"})).contains("src/main.rs"), "answered after the daemon was killed");
    let second = pid(&run(&ws, &["daemon", "status"]).1).expect("running again");
    assert_ne!(first, second);

    std::fs::write(ws.join("app/src/added.rs"), "fn added_later() {\n    todo!()\n}\n").unwrap();
    assert!(session.search(json!({"query": "added_later"})).contains("added.rs"));
    let before = records(&ws);
    let (ok, said) = run(&ws, &["daemon", "stop"]);
    assert!(ok && said.contains("stopped 1"), "{said}");
    assert_eq!(records(&ws), before + 1, "what was read again is on disk when stop returns");
    assert!(!run(&ws, &["daemon", "status"]).0, "not running after stop");
    assert!(session.search(json!({"query": "added_later"})).contains("added.rs"), "answered after stop");
    assert!(run(&ws, &["daemon", "status"]).0, "started again by the call");
    session.end();
    finish(&ws);
}

#[test]
fn a_disabled_daemon_leaves_sessions_to_answer_themselves() {
    let ws = workspace("disabled");
    let (ok, said) = run(&ws, &["daemon", "disable"]);
    assert!(ok, "{said}");
    let mut session = Session::start(&ws, &ws.join("app"));
    assert!(session.search(json!({"query": "start_app"})).contains("src/main.rs"));
    let (running, status) = run(&ws, &["daemon", "status"]);
    assert!(!running && status.contains("disabled"), "{status}");
    assert_eq!(pid_files(&ws), 0);
    session.end();
    let (ok, _) = run(&ws, &["daemon", "enable"]);
    assert!(ok);
    let (ok, said) = run(&ws, &["daemon", "start"]);
    assert!(ok && said.contains("started"), "{said}");
    assert!(run(&ws, &["daemon", "status"]).0);
    finish(&ws);
}

#[cfg(unix)]
#[test]
fn a_socket_directory_too_long_for_a_socket_is_passed_over() {
    let ws = workspace("long");
    let long = ws.join("run").join("x".repeat(120));
    std::fs::create_dir_all(&long).unwrap();
    let run_long = |args: &[&str]| {
        let output = omega(&ws).env("XDG_RUNTIME_DIR", &long).args(args).arg("--no-model").current_dir(ws.join("home")).output().unwrap();
        (output.status.success(), String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr))
    };
    let (ok, said) = run_long(&["daemon", "start"]);
    assert!(ok && said.contains("started"), "{said}");
    let (ok, said) = run_long(&["daemon", "status"]);
    assert!(ok && said.contains("/tmp/omega-"), "{said}");
    let (ok, said) = run_long(&["daemon", "stop"]);
    assert!(ok && said.contains("stopped 1"), "{said}");
    let _ = std::fs::remove_dir_all(&ws);
}
