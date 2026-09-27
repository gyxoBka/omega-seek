use crate::roots::{self, clean, shown};
use serde_json::{Map, Value};
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

const KEY: &str = "access";

#[derive(Debug, Default)]
pub struct Access {
    settings: Map<String, Value>,
    file: Option<PathBuf>,
}

impl Access {
    pub fn load() -> Result<Self, String> {
        let file = crate::paths::settings().ok_or("cannot tell where omega's settings are kept")?;
        Self::load_from(&file)
    }

    pub fn load_from(file: &Path) -> Result<Self, String> {
        let settings = match std::fs::read_to_string(file) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(Value::Object(settings)) => settings,
                _ => return Err(format!("{} is not a JSON object; fix or delete it", file.display())),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Map::new(),
            Err(error) => return Err(format!("{}: {error}", file.display())),
        };
        Ok(Self {
            settings,
            file: Some(file.to_path_buf()),
        })
    }

    pub fn save(&self) -> Result<(), String> {
        let file = self.file.as_deref().ok_or("these settings were not read from a file")?;
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(&Value::Object(self.settings.clone())).map_err(|error| error.to_string())?;
        let aside = file.with_extension("json.tmp");
        std::fs::write(&aside, format!("{text}\n")).map_err(|error| format!("{}: {error}", aside.display()))?;
        std::fs::rename(&aside, file).map_err(|error| format!("{}: {error}", file.display()))
    }

    #[must_use]
    pub fn cache_days(&self) -> Option<u64> {
        let days = self.settings.get("cache").and_then(|cache| cache.get("clean_after_days")).and_then(Value::as_u64);
        match days {
            Some(0) => None,
            Some(days) => Some(days),
            None => Some(crate::cache::DEFAULT_DAYS),
        }
    }

    pub fn set_cache_days(&mut self, days: Option<u64>) {
        let cache = self.settings.entry("cache").or_insert_with(|| Value::Object(Map::new()));
        if !cache.is_object() {
            *cache = Value::Object(Map::new());
        }
        if let Some(cache) = cache.as_object_mut() {
            cache.insert("clean_after_days".to_owned(), Value::from(days.unwrap_or(0)));
        }
    }

    #[must_use]
    pub fn daemon(&self) -> bool {
        self.settings.get("daemon").and_then(Value::as_bool).unwrap_or(true)
    }

    pub fn set_daemon(&mut self, enabled: bool) {
        self.settings.insert("daemon".to_owned(), Value::Bool(enabled));
    }

    #[must_use]
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    fn table(&self) -> Option<&Map<String, Value>> {
        self.settings.get(KEY)?.as_object()
    }

    fn table_mut(&mut self) -> &mut Map<String, Value> {
        let entry = self.settings.entry(KEY).or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        entry.as_object_mut().expect("just made an object")
    }

    #[must_use]
    pub fn granted(&self, repository: &Path) -> Vec<PathBuf> {
        let Some(table) = self.table() else { return Vec::new() };
        let key = shown(repository);
        table
            .iter()
            .filter(|(stored, _)| same(Path::new(stored.as_str()), Path::new(&key)))
            .filter_map(|(_, dirs)| dirs.as_array())
            .flatten()
            .filter_map(Value::as_str)
            .map(PathBuf::from)
            .collect()
    }

    #[must_use]
    pub fn repositories(&self) -> Vec<(PathBuf, Vec<PathBuf>)> {
        let Some(table) = self.table() else { return Vec::new() };
        table
            .iter()
            .map(|(repository, dirs)| {
                let dirs = dirs.as_array().into_iter().flatten().filter_map(Value::as_str).map(PathBuf::from).collect();
                (PathBuf::from(repository), dirs)
            })
            .collect()
    }

    pub fn grant(&mut self, repository: &Path, directory: &Path) -> bool {
        if self.granted(repository).iter().any(|dir| same(dir, directory)) {
            return false;
        }
        let key = self.key_of(repository);
        let dirs = self.table_mut().entry(key).or_insert_with(|| Value::Array(Vec::new()));
        if !dirs.is_array() {
            *dirs = Value::Array(Vec::new());
        }
        dirs.as_array_mut().expect("just made an array").push(Value::String(shown(directory)));
        true
    }

    pub fn revoke(&mut self, repository: &Path, directory: &Path) -> bool {
        let key = self.key_of(repository);
        let table = self.table_mut();
        let Some(dirs) = table.get_mut(&key).and_then(Value::as_array_mut) else {
            return false;
        };
        let before = dirs.len();
        dirs.retain(|dir| dir.as_str().is_none_or(|dir| !same(Path::new(dir), directory)));
        let changed = dirs.len() != before;
        if dirs.is_empty() {
            table.remove(&key);
        }
        changed
    }

    pub fn forget(&mut self, repository: &Path) -> bool {
        let key = self.key_of(repository);
        self.table_mut().remove(&key).is_some()
    }

    fn key_of(&self, repository: &Path) -> String {
        self.table()
            .and_then(|table| table.keys().find(|stored| same(Path::new(stored.as_str()), repository)).cloned())
            .unwrap_or_else(|| shown(repository))
    }

    #[must_use]
    pub fn allows(&self, home: &Path, top: &Path) -> bool {
        if top.starts_with(home) {
            return true;
        }
        let repository = roots::repository(home);
        if top.join(".git").exists() && roots::repository(top) == repository && repository.join(".git").exists() {
            return true;
        }
        self.granted(&repository).iter().any(|dir| top.starts_with(clean(dir)))
    }

    #[must_use]
    pub fn readable(&self, home: &Path) -> Vec<String> {
        let repository = roots::repository(home);
        let mut readable = vec![shown(home)];
        if repository.join(".git").is_dir() {
            if repository != home {
                readable.push(format!("{} (main checkout)", shown(&repository)));
            }
            for worktree in roots::linked_worktrees(&repository) {
                if worktree.path != home {
                    readable.push(format!("{} (worktree)", shown(&worktree.path)));
                }
            }
        }
        readable.extend(self.granted(&repository).iter().map(|dir| shown(dir)));
        readable
    }

    #[must_use]
    pub fn refusal(&self, home: &Path, top: &Path) -> String {
        let repository = roots::repository(home);
        format!(
            "`{top}` is outside this repository, and agents here have not been given access to it. \
             Ask the user to run, in {repository}:\n    omega access add \"{top}\"\n\
             Readable now: {readable}.",
            top = shown(top),
            repository = shown(&repository),
            readable = self.readable(home).join(", "),
        )
    }
}

fn same(a: &Path, b: &Path) -> bool {
    let (a, b) = (clean(a), clean(b));
    if cfg!(windows) {
        a.to_string_lossy().replace('\\', "/").to_lowercase() == b.to_string_lossy().replace('\\', "/").to_lowercase()
    } else {
        a == b
    }
}

pub fn run(arguments: &[String], all: bool) -> Result<(), String> {
    let mut access = Access::load()?;
    let subcommand = arguments.first().map(String::as_str);
    if all || subcommand == Some("forget") {
        return run_everywhere(&mut access, subcommand, arguments.get(1));
    }
    let here = std::env::current_dir().map_err(|error| error.to_string())?;
    let here = clean(&here.canonicalize().map_err(|error| error.to_string())?);
    let repository = roots::repository(&here);
    roots::refuse_if_too_wide(&repository).map_err(|_| {
        format!(
            "{} is not a repository. Run `omega access` in the repository an agent works in: what is \
             given there is what agents started there may read.",
            shown(&repository)
        )
    })?;
    match subcommand {
        Some("add") => {
            let directory = directory_argument(arguments.get(1), "add")?;
            roots::refuse_if_too_wide(&directory)?;
            if access.allows(&repository, &directory) {
                println!("  {} is readable from {} already.", shown(&directory), shown(&repository));
                return Ok(());
            }
            access.grant(&repository, &directory);
            access.save()?;
            println!("  Agents started in {} may now read {}.", shown(&repository), shown(&directory));
            Ok(())
        }
        Some("remove") => {
            let directory = directory_argument(arguments.get(1), "remove")?;
            if access.revoke(&repository, &directory) {
                access.save()?;
                println!("  Agents started in {} may no longer read {}.", shown(&repository), shown(&directory));
            } else {
                println!("  {} was not given {}.", shown(&repository), shown(&directory));
            }
            Ok(())
        }
        Some("list") => {
            list(&access, &repository);
            Ok(())
        }
        None if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() => take_back(&mut access, &repository),
        None => {
            list(&access, &repository);
            Ok(())
        }
        Some(other) => Err(format!("unknown `omega access {other}`; use add, remove, list, forget")),
    }
}

fn directory_argument(given: Option<&String>, subcommand: &str) -> Result<PathBuf, String> {
    let given = given.ok_or_else(|| format!("`omega access {subcommand}` needs a directory"))?;
    let path = Path::new(given);
    match path.canonicalize() {
        Ok(found) if found.is_dir() => Ok(clean(&found)),
        Ok(_) => Err(format!("{given} is not a directory")),
        Err(_) if subcommand == "remove" => Ok(clean(&std::env::current_dir().map_err(|error| error.to_string())?.join(path))),
        Err(_) => Err(format!("{given} does not exist")),
    }
}

fn list(access: &Access, repository: &Path) {
    let granted = access.granted(repository);
    if granted.is_empty() {
        println!("  Agents started in {} read only this repository and its worktrees.", shown(repository));
        println!("  Give access to another directory with: omega access add <dir>");
        return;
    }
    println!("  Agents started in {} (and its worktrees) may also read:", shown(repository));
    for dir in granted {
        let gone = if dir.is_dir() { "" } else { "  (not on disk)" };
        println!("    {}{gone}", shown(&dir));
    }
}

fn take_back(access: &mut Access, repository: &Path) -> Result<(), String> {
    let granted = access.granted(repository);
    if granted.is_empty() {
        list(access, repository);
        return Ok(());
    }
    let labels: Vec<String> = granted.iter().map(|dir| shown(dir)).collect();
    let picked = inquire::MultiSelect::new(
        &format!("Agents started in {} may read these. Select any to take back:", shown(repository)),
        labels,
    )
    .prompt()
    .map_err(|error| error.to_string())?;
    if picked.is_empty() {
        println!("  Nothing changed.");
        return Ok(());
    }
    for label in &picked {
        access.revoke(repository, Path::new(label));
    }
    access.save()?;
    for label in picked {
        println!("  Taken back: {label}");
    }
    Ok(())
}

fn run_everywhere(access: &mut Access, subcommand: Option<&str>, argument: Option<&String>) -> Result<(), String> {
    if subcommand == Some("forget") {
        let given = argument.ok_or("`omega access forget` needs the repository whose access to forget")?;
        let repository = Path::new(given).canonicalize().map(|found| clean(&found)).unwrap_or_else(|_| PathBuf::from(given));
        if access.forget(&repository) {
            access.save()?;
            println!("  Forgot the access given to {}.", shown(&repository));
        } else {
            println!("  {} was given no access.", shown(&repository));
        }
        return Ok(());
    }
    let repositories = access.repositories();
    if repositories.is_empty() {
        println!("  No repository has been given access outside itself.");
    }
    for (repository, dirs) in repositories {
        let gone = if repository.is_dir() { "" } else { "  (not on disk: `omega access forget` it)" };
        println!("  {}{gone}", shown(&repository));
        for dir in dirs {
            let gone = if dir.is_dir() { "" } else { "  (not on disk)" };
            println!("    {}{gone}", shown(&dir));
        }
    }
    if let Some(file) = access.file() {
        println!("\n  Settings: {}", file.display());
    }
    Ok(())
}
