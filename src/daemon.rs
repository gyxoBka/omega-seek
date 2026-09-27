use crate::access::Access;
use crate::engine::Engine;
use interprocess::local_socket::{ListenerOptions, Name, Stream, prelude::*};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

const MOST_ROOTS: usize = 16;
const START_WAIT: Duration = Duration::from_secs(3);
const STOP_WAIT: Duration = Duration::from_secs(10);
const IDLE: Duration = Duration::from_secs(30 * 60);

fn identity(model: Option<&Path>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
    hasher.update([0]);
    hasher.update(model.map(|dir| dir.to_string_lossy().into_owned()).unwrap_or_default().as_bytes());
    hasher.update([0]);
    hasher.update(crate::paths::state().map(|dir| dir.to_string_lossy().into_owned()).unwrap_or_default().as_bytes());
    hasher.finalize().iter().take(8).map(|byte| format!("{byte:02x}")).collect()
}

fn user() -> String {
    let name = std::env::var("USERNAME").or_else(|_| std::env::var("USER")).unwrap_or_default();
    name.chars().filter(char::is_ascii_alphanumeric).collect()
}

#[cfg(windows)]
fn name(id: &str) -> std::io::Result<Name<'static>> {
    use interprocess::local_socket::GenericNamespaced;
    format!("omega-{}-{id}", user()).to_ns_name::<GenericNamespaced>()
}

#[cfg(unix)]
fn name(id: &str) -> std::io::Result<Name<'static>> {
    use interprocess::local_socket::GenericFilePath;
    socket_path(id)?.to_fs_name::<GenericFilePath>()
}

#[cfg(unix)]
fn socket_path(id: &str) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = crate::paths::sockets().ok_or_else(|| std::io::Error::other("no directory for the socket"))?;
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir.join(format!("daemon-{id}.sock")))
}

fn shown_name(id: &str) -> String {
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\omega-{}-{id}", user())
    }
    #[cfg(unix)]
    {
        socket_path(id).map(|path| path.display().to_string()).unwrap_or_default()
    }
}

fn pid_file(id: &str) -> Option<PathBuf> {
    Some(crate::paths::state()?.join(format!("daemon-{id}.pid")))
}

#[derive(Debug, Clone)]
struct Recorded {
    id: String,
    pid: u32,
    version: String,
}

fn recorded() -> Vec<Recorded> {
    let Some(dir) = crate::paths::state() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let id = name.strip_prefix("daemon-")?.strip_suffix(".pid")?.to_owned();
            let text = std::fs::read_to_string(entry.path()).ok()?;
            let value: Value = serde_json::from_str(&text).ok()?;
            Some(Recorded {
                id,
                pid: u32::try_from(value["pid"].as_u64()?).ok()?,
                version: value["version"].as_str().unwrap_or_default().to_owned(),
            })
        })
        .collect()
}

fn alive(pid: u32) -> bool {
    if cfg!(windows) {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\"")))
    } else {
        Command::new("kill").args(["-0", &pid.to_string()]).stderr(Stdio::null()).status().is_ok_and(|status| status.success())
    }
}

fn kill(pid: u32) {
    let _ = if cfg!(windows) {
        Command::new("taskkill").args(["/F", "/PID", &pid.to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status()
    } else {
        Command::new("kill").args(["-9", &pid.to_string()]).stderr(Stdio::null()).status()
    };
}

#[must_use]
pub fn wanted() -> bool {
    std::env::var_os("OMEGA_NO_DAEMON").is_none() && Access::load().map_or(true, |settings| settings.daemon())
}

#[derive(Debug)]
pub struct Link {
    reader: BufReader<Stream>,
}

impl Link {
    fn connect(id: &str) -> Option<Self> {
        let stream = Stream::connect(name(id).ok()?).ok()?;
        Some(Self { reader: BufReader::new(stream) })
    }

    fn request(&mut self, message: &Value) -> std::io::Result<Value> {
        let mut line = message.to_string();
        line.push('\n');
        self.reader.get_mut().write_all(line.as_bytes())?;
        let mut reply = String::new();
        if self.reader.read_line(&mut reply)? == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the daemon closed the connection"));
        }
        serde_json::from_str(&reply).map_err(std::io::Error::other)
    }

    pub fn call(&mut self, home: &Path, params: &Value) -> std::io::Result<Value> {
        let reply = self.request(&json!({"call": params, "home": home}))?;
        Ok(reply["result"].clone())
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn keep_own_handles() {
    unsafe extern "system" {
        fn GetStdHandle(which: u32) -> isize;
        fn SetHandleInformation(handle: isize, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;
    for which in [-10i32, -11, -12] {
        unsafe {
            let handle = GetStdHandle(which as u32);
            if handle != 0 && handle != -1 {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

fn spawn(model: Option<&Path>) -> std::io::Result<()> {
    let mut command = Command::new(std::env::current_exe()?);
    command.arg("daemon");
    match model {
        Some(dir) => command.arg("--model").arg(dir),
        None => command.arg("--no-model"),
    };
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    let place = crate::paths::state().filter(|dir| std::fs::create_dir_all(dir).is_ok()).unwrap_or_else(std::env::temp_dir);
    command.current_dir(place);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        keep_own_handles();
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        if command.spawn().is_ok() {
            return Ok(());
        }
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    command.spawn().map(|_| ())
}

fn connect_or_start(id: &str, model: Option<&Path>) -> Option<Link> {
    if let Some(link) = Link::connect(id) {
        return Some(link);
    }
    spawn(model).ok()?;
    let started = Instant::now();
    let mut pause = Duration::from_millis(10);
    while started.elapsed() < START_WAIT {
        std::thread::sleep(pause);
        if let Some(link) = Link::connect(id) {
            return Some(link);
        }
        pause = (pause * 2).min(Duration::from_millis(200));
    }
    None
}

#[must_use]
pub fn attach(home: &Path, model: Option<&Path>) -> Option<Link> {
    let mut link = connect_or_start(&identity(model), model)?;
    link.request(&json!({"hello": home})).ok()?;
    Some(link)
}

struct Daemon {
    engine: Engine,
    clients: AtomicUsize,
    last: Mutex<Instant>,
    started: Instant,
    id: String,
    model: Option<PathBuf>,
}

impl Daemon {
    fn status(&self) -> Value {
        let roots: Vec<Value> = self
            .engine
            .open()
            .into_iter()
            .map(|open| json!({"root": crate::roots::shown(&open.root), "files": open.files, "chunks": open.chunks, "busy": open.busy}))
            .collect();
        json!({
            "pid": std::process::id(),
            "version": env!("CARGO_PKG_VERSION"),
            "id": self.id,
            "up": self.started.elapsed().as_secs(),
            "clients": self.clients.load(Ordering::SeqCst),
            "roots": roots,
            "model": self.model.as_ref().map(|dir| dir.display().to_string()),
            "socket": shown_name(&self.id),
        })
    }

    fn leave(&self) -> ! {
        self.engine.persist();
        if let Some(file) = pid_file(&self.id) {
            let _ = std::fs::remove_file(file);
        }
        #[cfg(unix)]
        if let Ok(path) = socket_path(&self.id) {
            let _ = std::fs::remove_file(path);
        }
        std::process::exit(0)
    }

    fn serve(&self, stream: Stream) {
        let mut homes: Vec<PathBuf> = Vec::new();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
            let reply = if let Some(home) = message["hello"].as_str() {
                let home = PathBuf::from(home);
                if homes.is_empty() {
                    self.clients.fetch_add(1, Ordering::SeqCst);
                }
                self.engine.pin(&home);
                self.engine.prepare(&home);
                homes.push(home);
                json!({"ok": true})
            } else if message.get("call").is_some() {
                let home = PathBuf::from(message["home"].as_str().unwrap_or_default());
                json!({"result": self.engine.call(&home, &message["call"])})
            } else {
                match message["control"].as_str() {
                    Some("status") => json!({"status": self.status()}),
                    Some("stop") => {
                        self.engine.persist();
                        let _ = reader.get_mut().write_all(format!("{}\n", json!({"stopped": true})).as_bytes());
                        let _ = reader.get_mut().flush();
                        self.leave();
                    }
                    _ => json!({"error": "unknown request"}),
                }
            };
            if reader.get_mut().write_all(format!("{reply}\n").as_bytes()).is_err() {
                break;
            }
        }
        if !homes.is_empty() {
            self.clients.fetch_sub(1, Ordering::SeqCst);
        }
        for home in homes {
            self.engine.unpin(&home);
        }
        if let Ok(mut last) = self.last.lock() {
            *last = Instant::now();
        }
    }
}

fn listen(id: &str) -> Result<interprocess::local_socket::Listener, String> {
    let options = || name(id).map(|name| ListenerOptions::new().name(name));
    match options().and_then(ListenerOptions::create_sync) {
        Ok(listener) => Ok(listener),
        Err(_) if Link::connect(id).is_some() => Err("another omega daemon is running".to_owned()),
        Err(error) => {
            #[cfg(unix)]
            if let Ok(path) = socket_path(id) {
                let _ = std::fs::remove_file(path);
                return options().and_then(ListenerOptions::create_sync).map_err(|error| error.to_string());
            }
            Err(error.to_string())
        }
    }
}

pub fn run(model: Option<&Path>) -> Result<(), String> {
    let id = identity(model);
    let listener = listen(&id)?;
    if let Some(file) = pid_file(&id) {
        if let Some(parent) = file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&file, json!({"pid": std::process::id(), "version": env!("CARGO_PKG_VERSION")}).to_string());
    }
    std::thread::spawn(|| {
        if let Some(dir) = crate::paths::stores() {
            let _ = crate::store::prune(&dir, crate::store::ABANDONED);
        }
    });
    let idle = std::env::var("OMEGA_DAEMON_IDLE_SECS").ok().and_then(|secs| secs.parse().ok()).map_or(IDLE, Duration::from_secs);
    let daemon = Arc::new(Daemon {
        engine: Engine::new(model.map(Path::to_path_buf), MOST_ROOTS),
        clients: AtomicUsize::new(0),
        last: Mutex::new(Instant::now()),
        started: Instant::now(),
        id,
        model: model.map(Path::to_path_buf),
    });
    let watchdog = Arc::clone(&daemon);
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1).min(idle));
            let quiet = watchdog.last.lock().map(|last| last.elapsed() >= idle).unwrap_or(false);
            if watchdog.clients.load(Ordering::SeqCst) == 0 && quiet {
                watchdog.leave();
            }
        }
    });
    for stream in listener.incoming().flatten() {
        let daemon = Arc::clone(&daemon);
        std::thread::spawn(move || daemon.serve(stream));
    }
    Ok(())
}

fn ask(id: &str, message: Value, wait: Duration) -> Option<Value> {
    let mut link = Link::connect(id)?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(link.request(&message));
    });
    receiver.recv_timeout(wait).ok()?.ok()
}

fn stop_one(id: &str, pid: Option<u32>) -> bool {
    let stopped = ask(id, json!({"control": "stop"}), STOP_WAIT).is_some_and(|reply| reply["stopped"] == json!(true));
    if !stopped {
        if let Some(pid) = pid.filter(|&pid| alive(pid)) {
            kill(pid);
        } else if Link::connect(id).is_none() {
            return false;
        }
    }
    if let Some(file) = pid_file(id) {
        let _ = std::fs::remove_file(file);
    }
    true
}

pub fn stop_all() -> usize {
    recorded().into_iter().filter(|daemon| stop_one(&daemon.id, Some(daemon.pid))).count()
}

pub fn retire_idle() {
    for daemon in recorded() {
        let idle = ask(&daemon.id, json!({"control": "status"}), Duration::from_secs(2))
            .is_some_and(|reply| reply["status"]["clients"].as_u64() == Some(0));
        if idle {
            stop_one(&daemon.id, Some(daemon.pid));
        }
    }
}

fn uptime(seconds: u64) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
    }
}

fn print_status(status: &Value) {
    println!(
        "  omega daemon {} (pid {}), up {}, {} session{}",
        status["version"].as_str().unwrap_or_default(),
        status["pid"],
        uptime(status["up"].as_u64().unwrap_or_default()),
        status["clients"],
        if status["clients"].as_u64() == Some(1) { "" } else { "s" },
    );
    println!("  socket: {}", status["socket"].as_str().unwrap_or_default());
    let roots = status["roots"].as_array().cloned().unwrap_or_default();
    if roots.is_empty() {
        println!("  no repository open");
    }
    for root in roots {
        if root["busy"] == json!(true) {
            println!("    {}  (answering or indexing)", root["root"].as_str().unwrap_or_default());
        } else {
            println!("    {}  {} files, {} chunks", root["root"].as_str().unwrap_or_default(), root["files"], root["chunks"]);
        }
    }
}

pub fn command(subcommand: Option<&str>, all: bool, model: Option<&Path>) -> Result<(), String> {
    let id = identity(model);
    match subcommand {
        None => run(model),
        Some("status") => {
            let mine = ask(&id, json!({"control": "status"}), Duration::from_secs(5));
            let others: Vec<Recorded> = recorded().into_iter().filter(|daemon| daemon.id != id && alive(daemon.pid)).collect();
            for daemon in &others {
                println!("  also running: omega daemon {} (pid {})", daemon.version, daemon.pid);
            }
            if !wanted() {
                println!("  the daemon is disabled: sessions run in their own process (`omega daemon enable`)");
            }
            match mine {
                Some(reply) => {
                    print_status(&reply["status"]);
                    Ok(())
                }
                None => Err("  omega daemon is not running".to_owned()),
            }
        }
        Some("stop") => {
            let stopped = if all { stop_all() } else { usize::from(stop_one(&id, recorded().iter().find(|daemon| daemon.id == id).map(|daemon| daemon.pid))) };
            println!("  {}", if stopped == 0 { "omega daemon is not running".to_owned() } else { format!("stopped {stopped} daemon{}", if stopped == 1 { "" } else { "s" }) });
            Ok(())
        }
        Some("start") => {
            if Link::connect(&id).is_some() {
                println!("  omega daemon is running already");
                return Ok(());
            }
            connect_or_start(&id, model).ok_or("the daemon did not start")?;
            println!("  omega daemon started");
            Ok(())
        }
        Some("restart") => {
            stop_one(&id, recorded().iter().find(|daemon| daemon.id == id).map(|daemon| daemon.pid));
            connect_or_start(&id, model).ok_or("the daemon did not start")?;
            println!("  omega daemon restarted");
            Ok(())
        }
        Some(toggle @ ("enable" | "disable")) => {
            let mut settings = Access::load()?;
            settings.set_daemon(toggle == "enable");
            settings.save()?;
            if toggle == "disable" {
                let stopped = stop_all();
                println!("  daemon disabled: sessions run in their own process{}", if stopped > 0 { "; the running daemon was stopped" } else { "" });
            } else {
                println!("  daemon enabled: it starts with the next session");
            }
            Ok(())
        }
        Some(other) => Err(format!("unknown `omega daemon {other}`; use status, start, stop, restart, enable, disable")),
    }
}
