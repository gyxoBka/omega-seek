use super::agents::{self, Agent, Dirs};
use super::{Integration, Mode, Request, apply, describe, display, installed, select};
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::Command;

const MARK: &str = "# added by omega";
const PROFILES: [&str; 4] = [".zshrc", ".bashrc", ".bash_profile", ".profile"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Part {
    Integrations,
    Stores,
    Model,
    Settings,
    PathEntry,
    Binary,
}

impl Part {
    pub const ALL: [Self; 6] = [Self::Integrations, Self::Stores, Self::Model, Self::Settings, Self::PathEntry, Self::Binary];

    fn label(self) -> &'static str {
        match self {
            Self::Integrations => "Agent integrations",
            Self::Stores => "Index caches",
            Self::Model => "Model",
            Self::Settings => "Settings",
            Self::PathEntry => "PATH entry",
            Self::Binary => "Binary",
        }
    }

    fn data(self) -> bool {
        matches!(self, Self::Stores | Self::Model | Self::Settings)
    }
}

#[derive(Debug, Default)]
pub struct Found {
    pub agents: Vec<String>,
    pub stores: Option<(PathBuf, u64, usize)>,
    pub model: Option<(PathBuf, u64)>,
    pub settings: Option<(PathBuf, usize)>,
    pub path_entry: Option<String>,
    pub binary: Option<PathBuf>,
    pub foreign_binary: Option<PathBuf>,
}

impl Found {
    fn describe(&self, part: Part) -> Option<String> {
        match part {
            Part::Integrations => (!self.agents.is_empty()).then(|| {
                let count = self.agents.len();
                format!("{} ({count} agent{})", self.agents.join(", "), if count == 1 { "" } else { "s" })
            }),
            Part::Stores => self.stores.as_ref().map(|(dir, bytes, count)| {
                format!("{}, {count} repositor{}   ({})", size(*bytes), if *count == 1 { "y" } else { "ies" }, dir.display())
            }),
            Part::Model => self.model.as_ref().map(|(dir, bytes)| format!("{}   ({})", size(*bytes), dir.display())),
            Part::Settings => self.settings.as_ref().map(|(file, grants)| {
                format!("access given to {grants} repositor{}   ({})", if *grants == 1 { "y" } else { "ies" }, file.display())
            }),
            Part::PathEntry => self.path_entry.clone(),
            Part::Binary => self.binary.as_ref().map(|path| path.display().to_string()),
        }
    }

    #[must_use]
    pub fn present(&self) -> Vec<Part> {
        Part::ALL.into_iter().filter(|&part| self.describe(part).is_some()).collect()
    }
}

#[must_use]
pub fn ticked(found: &Found, request: &Request, keep_data: bool) -> Vec<Part> {
    let present = found.present();
    if request.agents.is_some() || request.integrations.is_some() {
        return present.into_iter().filter(|&part| part == Part::Integrations).collect();
    }
    present.into_iter().filter(|part| !(keep_data && part.data())).collect()
}

#[must_use]
pub fn warnings(found: &Found, parts: &[Part], agents_left: &[String]) -> Vec<String> {
    let mut warnings = Vec::new();
    if parts.contains(&Part::Binary) {
        let binary = found.binary.as_ref().map(|path| path.display().to_string()).unwrap_or_default();
        for agent in agents_left {
            warnings.push(format!("{agent} will still start {binary}, which will be gone: its MCP server will fail"));
        }
        if found.path_entry.is_some() && !parts.contains(&Part::PathEntry) {
            warnings.push("the PATH entry will name a directory without omega in it".to_owned());
        }
    }
    if parts.contains(&Part::Model) && !parts.contains(&Part::Binary) && found.binary.is_some() {
        warnings.push("without the model, search is lexical only until `omega model install`".to_owned());
    }
    warnings
}

#[must_use]
pub fn without_directory(value: &str, dir: &str) -> Option<String> {
    let same = |entry: &str| entry.trim_end_matches(['\\', '/']).eq_ignore_ascii_case(dir.trim_end_matches(['\\', '/']));
    let entries: Vec<&str> = value.split(';').filter(|entry| !entry.is_empty()).collect();
    let kept: Vec<&str> = entries.iter().copied().filter(|entry| !same(entry)).collect();
    (kept.len() != entries.len()).then(|| kept.join(";"))
}

#[must_use]
pub fn without_marked_lines(text: &str) -> Option<String> {
    if !text.contains(MARK) {
        return None;
    }
    let mut kept: String = text.lines().filter(|line| !line.contains(MARK)).collect::<Vec<_>>().join("\n");
    if text.ends_with('\n') {
        kept.push('\n');
    }
    Some(kept)
}

fn size(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64)
    } else {
        format!("{:.0} MB", (bytes as f64 / (1u64 << 20) as f64).max(if bytes > 0 { 1.0 } else { 0.0 }))
    }
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| match entry.metadata() {
                    Ok(meta) if meta.is_dir() => dir_size(&entry.path()),
                    Ok(meta) => meta.len(),
                    Err(_) => 0,
                })
                .sum()
        })
        .unwrap_or(0)
}

fn ours(binary: &Path) -> bool {
    !binary.components().any(|part| matches!(part.as_os_str().to_str(), Some(".cargo" | "target")))
}

fn user_path() -> Option<String> {
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", "[Environment]::GetEnvironmentVariable('Path', 'User')"])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn set_user_path(value: &str) -> bool {
    Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", "[Environment]::SetEnvironmentVariable('Path', $env:OMEGA_USER_PATH, 'User')"])
        .env("OMEGA_USER_PATH", value)
        .status()
        .is_ok_and(|status| status.success())
}

fn marked_profiles(dirs: &Dirs) -> Vec<PathBuf> {
    PROFILES
        .iter()
        .map(|name| dirs.home.join(name))
        .filter(|profile| std::fs::read_to_string(profile).is_ok_and(|text| text.contains(MARK)))
        .collect()
}

fn look(dirs: &Dirs, known: &[Agent], exe: &Path) -> Found {
    let mut found = Found {
        agents: known
            .iter()
            .filter(|agent| Integration::ALL.iter().any(|&integration| installed(agent, integration)))
            .map(|agent| agent.name.to_owned())
            .collect(),
        ..Found::default()
    };
    if let Some(dir) = crate::paths::stores().filter(|dir| dir.is_dir()) {
        let count = std::fs::read_dir(&dir)
            .map(|entries| entries.flatten().filter(|entry| entry.path().extension().is_some_and(|ext| ext == "idx" || ext == "bin")).count())
            .unwrap_or(0);
        found.stores = Some((dir.clone(), dir_size(&dir), count));
    }
    if let Some(dir) = crate::paths::models().filter(|dir| dir.is_dir()) {
        found.model = Some((dir.clone(), dir_size(&dir)));
    }
    if let Some(file) = crate::paths::settings().filter(|file| file.is_file()) {
        let grants = crate::access::Access::load_from(&file).map(|access| access.repositories().len()).unwrap_or(0);
        found.settings = Some((file, grants));
    }
    if ours(exe) {
        found.binary = Some(exe.to_path_buf());
        let dir = exe.parent().map(|dir| dir.display().to_string()).unwrap_or_default();
        found.path_entry = if cfg!(windows) {
            user_path().and_then(|value| without_directory(&value, &dir)).map(|_| dir)
        } else {
            let profiles = marked_profiles(dirs);
            (!profiles.is_empty()).then(|| profiles.iter().map(|profile| display(dirs, profile)).collect::<Vec<_>>().join(", "))
        };
    } else {
        found.foreign_binary = Some(exe.to_path_buf());
    }
    found
}

pub fn run(request: Request, keep_data: bool) -> Result<(), String> {
    let dirs = Dirs::discover()?;
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let known = agents::agents(&dirs);
    println!("\n  omega Uninstaller\n");
    let found = look(&dirs, &known, &exe);
    let asking = !request.yes && !request.dry_run;
    if asking && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return Err("not a terminal: pass --yes to remove everything, or --dry-run to see what would go".to_owned());
    }
    let present = found.present();
    if present.is_empty() {
        println!("  Nothing of omega's is left to remove.\n");
        return Ok(());
    }
    let mut parts = ticked(&found, &request, keep_data);
    if asking && request.agents.is_none() && request.integrations.is_none() {
        let labels: Vec<String> = present
            .iter()
            .map(|&part| format!("{:<20} {}", part.label(), found.describe(part).unwrap_or_default()))
            .collect();
        let defaults: Vec<usize> = (0..present.len()).filter(|&at| parts.contains(&present[at])).collect();
        parts = select("Select what to remove:", labels, &defaults)?.into_iter().map(|at| present[at]).collect();
    }
    let with_ours: Vec<&Agent> = known.iter().filter(|agent| Integration::ALL.iter().any(|&integration| installed(agent, integration))).collect();
    let (chosen, integrations): (Vec<&Agent>, Vec<Integration>) = if parts.contains(&Part::Integrations) {
        let chosen: Vec<&Agent> = match &request.agents {
            Some(ids) => with_ours.iter().copied().filter(|agent| ids.iter().any(|id| id == agent.id)).collect(),
            None if asking => {
                let labels = with_ours.iter().map(|agent| agent.name.to_owned()).collect();
                let all: Vec<usize> = (0..with_ours.len()).collect();
                select("Select agents to remove omega from:", labels, &all)?.into_iter().map(|at| with_ours[at]).collect()
            }
            None => with_ours.clone(),
        };
        let integrations = match &request.integrations {
            Some(integrations) => integrations.clone(),
            None if asking => {
                let labels = Integration::ALL.iter().map(|integration| integration.label().to_owned()).collect();
                select("Select integrations to remove:", labels, &[0, 1, 2])?.into_iter().map(|at| Integration::ALL[at]).collect()
            }
            None => Integration::ALL.to_vec(),
        };
        (chosen, integrations)
    } else {
        (Vec::new(), Vec::new())
    };
    let left: Vec<String> = with_ours
        .iter()
        .filter(|agent| !chosen.iter().any(|picked| picked.id == agent.id) || !integrations.contains(&Integration::Mcp))
        .filter(|agent| installed(agent, Integration::Mcp))
        .map(|agent| agent.name.to_owned())
        .collect();

    println!("\n  Plan:\n");
    for &part in &parts {
        if part == Part::Integrations {
            println!("  {:<20} {}", part.label(), chosen.iter().map(|agent| agent.name).collect::<Vec<_>>().join(", "));
        } else {
            println!("  {:<20} {}", part.label(), found.describe(part).unwrap_or_default());
        }
    }
    if let Some(foreign) = &found.foreign_binary {
        let built = foreign.components().any(|part| part.as_os_str() == "target");
        let how = if built { "it is a build in a source checkout, which `cargo clean` removes" } else { "remove it with what installed it (`cargo uninstall omega-seek`)" };
        println!("\n  The binary {} was not put there by omega's installer: {how}.", foreign.display());
    }
    for warning in warnings(&found, &parts, &left) {
        println!("\n  Note: {warning}");
    }
    println!();
    if parts.is_empty() {
        println!("  Nothing selected.\n");
        return Ok(());
    }
    if request.dry_run {
        println!("  Dry run: nothing was removed.\n");
        return Ok(());
    }
    if asking {
        let proceed = inquire::Confirm::new("Proceed?").with_default(true).prompt().map_err(|error| error.to_string())?;
        if !proceed {
            println!("  Nothing changed.");
            return Ok(());
        }
    }
    remove(&dirs, &found, &parts, &chosen, &integrations, &exe);
    Ok(())
}

fn remove(dirs: &Dirs, found: &Found, parts: &[Part], chosen: &[&Agent], integrations: &[Integration], exe: &Path) {
    if parts.iter().any(|part| matches!(part, Part::Binary | Part::Stores | Part::Model)) {
        let stopped = crate::daemon::stop_all();
        if stopped > 0 {
            println!("  daemon               stopped {stopped}");
        }
    }
    for agent in chosen {
        for &integration in integrations {
            let Some(path) = integration.target(agent) else { continue };
            let action = apply(Mode::Uninstall, agent, integration, exe);
            println!("  {:<20} {:<14} {}", format!("{} {}", agent.name, integration.label()), describe(&action), display(dirs, path));
        }
    }
    if parts.contains(&Part::Stores) {
        if let Some((dir, bytes, _)) = &found.stores {
            let removed = std::fs::remove_dir_all(dir).is_ok();
            println!("  index caches         {} {}", if removed { format!("removed {}", size(*bytes)) } else { "could not remove".to_owned() }, dir.display());
        }
        for dir in [crate::paths::state(), crate::paths::sockets()].into_iter().flatten() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().starts_with("daemon-") {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
            let _ = std::fs::remove_dir(&dir);
        }
    }
    if parts.contains(&Part::Model) {
        if let Some((dir, bytes)) = &found.model {
            let removed = std::fs::remove_dir_all(dir).is_ok();
            println!("  model                {} {}", if removed { format!("removed {}", size(*bytes)) } else { "could not remove".to_owned() }, dir.display());
            if let Some(parent) = dir.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }
    if parts.contains(&Part::Settings) {
        if let Some((file, _)) = &found.settings {
            let removed = std::fs::remove_file(file).is_ok();
            let _ = std::fs::remove_file(file.with_extension("json.tmp"));
            if let Some(parent) = file.parent() {
                let _ = std::fs::remove_dir(parent);
            }
            println!("  settings             {} {}", if removed { "removed" } else { "could not remove" }, file.display());
        }
    }
    if parts.contains(&Part::PathEntry) {
        remove_path_entry(dirs, exe);
    }
    if parts.contains(&Part::Binary) {
        remove_binary(exe);
    }
    println!();
}

fn remove_path_entry(dirs: &Dirs, exe: &Path) {
    if cfg!(windows) {
        let dir = exe.parent().map(|dir| dir.display().to_string()).unwrap_or_default();
        match user_path().and_then(|value| without_directory(&value, &dir)) {
            Some(value) if set_user_path(&value) => println!("  PATH entry           removed {dir} (new terminals see it)"),
            Some(_) => println!("  PATH entry           could not change the user PATH; remove {dir} from it by hand"),
            None => {}
        }
        return;
    }
    for profile in marked_profiles(dirs) {
        let changed = std::fs::read_to_string(&profile).ok().and_then(|text| without_marked_lines(&text));
        if let Some(text) = changed {
            if std::fs::write(&profile, text).is_ok() {
                println!("  PATH entry           removed from {}", display(dirs, &profile));
            }
        }
    }
}

fn remove_binary(exe: &Path) {
    let Some(dir) = exe.parent() else { return };
    let name = exe.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&format!("{name}.old")) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    if !cfg!(windows) {
        match std::fs::remove_file(exe) {
            Ok(()) => println!("  binary               removed {}", exe.display()),
            Err(error) => println!("  binary               could not remove {}: {error}", exe.display()),
        }
        return;
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs());
    let aside = exe.with_file_name(format!("{name}.old-{stamp}"));
    if let Err(error) = std::fs::rename(exe, &aside) {
        println!("  binary               could not remove {}: {error}", exe.display());
        return;
    }
    if delete_later(&aside, dir) {
        println!("  binary               removed {}", exe.display());
    } else {
        println!("  binary               delete {} once this command ends", aside.display());
    }
}

#[cfg(windows)]
fn delete_later(file: &Path, dir: &Path) -> bool {
    use std::os::windows::process::CommandExt as _;
    let mut command = Command::new("cmd");
    command.raw_arg(format!(
        "/s /c \"ping -n 3 127.0.0.1 >nul & del /f /q \"{}\" & rmdir \"{}\"\"",
        file.display(),
        dir.display()
    ));
    crate::daemon::spawn_detached(command, &std::env::temp_dir()).is_ok()
}

#[cfg(not(windows))]
fn delete_later(_file: &Path, _dir: &Path) -> bool {
    false
}
