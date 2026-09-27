use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resolved {
    pub root: PathBuf,
    pub within: Option<String>,
    pub elsewhere: bool,
}

pub fn resolve(home: &Path, root: Option<&str>, path: Option<&str>) -> Result<Resolved, String> {
    let home = clean(&home.canonicalize().map_err(|error| format!("{}: {error}", home.display()))?);
    let asked = root.map(str::trim).filter(|root| !root.is_empty());
    let path = path.map(str::trim).filter(|path| !path.is_empty());

    if let Some(asked) = asked {
        let given = Path::new(asked);
        let joined = if given.is_absolute() { given.to_path_buf() } else { home.join(given) };
        let target = joined
            .canonicalize()
            .map(|found| clean(&found))
            .map_err(|_| format!("root `{asked}` does not exist (looked for {})", shown(&joined)))?;
        let directory = if target.is_dir() { target } else { parent_of(&target) };
        let (top, inside) = checkout_of(&directory);
        refuse_if_too_wide(&top)?;
        let within = match (inside, path.filter(|path| !Path::new(path).is_absolute())) {
            (Some(inside), Some(path)) => Some(format!("{inside}/{}", path.replace('\\', "/"))),
            (Some(inside), None) => Some(inside),
            (None, path) => path.map(|path| path.replace('\\', "/")),
        };
        return Ok(Resolved {
            elsewhere: top != home,
            root: top,
            within,
        });
    }

    if let Some(path) = path.filter(|path| Path::new(path).is_absolute()) {
        let mut existing = clean(Path::new(path));
        while !existing.exists() {
            existing = parent_of(&existing);
        }
        let existing = clean(&existing.canonicalize().unwrap_or(existing));
        let directory = if existing.is_dir() { existing } else { parent_of(&existing) };
        let (nearest, _) = checkout_of(&directory);
        let checkout = nearest.join(".git").exists().then_some(nearest);
        let top = match checkout {
            Some(nested) if nested != home && nested.starts_with(&home) && stands_apart(&home, &nested) => nested,
            _ if directory.starts_with(&home) => home.clone(),
            Some(elsewhere) => elsewhere,
            None => {
                return Err(format!(
                    "`{path}` is outside {} and inside no repository; pass `root` to search elsewhere",
                    home.display()
                ));
            }
        };
        refuse_if_too_wide(&top)?;
        let within = relative(&top, &clean(Path::new(path)));
        return Ok(Resolved {
            elsewhere: top != home,
            root: top,
            within,
        });
    }

    Ok(Resolved {
        root: home,
        within: path.map(|path| path.replace('\\', "/")),
        elsewhere: false,
    })
}

#[must_use]
pub fn common_git_dir(checkout: &Path) -> Option<PathBuf> {
    let dot = checkout.join(".git");
    if dot.is_dir() {
        return Some(clean(&dot.canonicalize().ok()?));
    }
    let pointer = std::fs::read_to_string(&dot).ok()?;
    let gitdir = pointer.lines().find_map(|line| line.strip_prefix("gitdir:"))?.trim();
    let gitdir = checkout.join(gitdir);
    let common = match std::fs::read_to_string(gitdir.join("commondir")) {
        Ok(common) => gitdir.join(common.trim()),
        Err(_) => gitdir,
    };
    Some(clean(&common.canonicalize().ok()?))
}

#[must_use]
pub fn repository(directory: &Path) -> PathBuf {
    let (top, _) = checkout_of(directory);
    if !top.join(".git").exists() {
        return directory.to_path_buf();
    }
    match common_git_dir(&top) {
        Some(common) if common.file_name().is_some_and(|name| name == ".git") => parent_of(&common),
        _ => top,
    }
}

fn checkout_of(directory: &Path) -> (PathBuf, Option<String>) {
    let mut at = Some(directory);
    while let Some(candidate) = at {
        if candidate.join(".git").exists() {
            return (candidate.to_path_buf(), relative(candidate, directory));
        }
        at = candidate.parent();
    }
    (directory.to_path_buf(), None)
}

fn stands_apart(home: &Path, nested: &Path) -> bool {
    let hidden = nested
        .strip_prefix(home)
        .is_ok_and(|rest| rest.components().any(|part| part.as_os_str().to_string_lossy().starts_with('.')));
    hidden || home.join(".git").exists()
}

fn relative(top: &Path, inside: &Path) -> Option<String> {
    let rest = inside.strip_prefix(top).ok()?;
    let text = rest.to_string_lossy().replace('\\', "/");
    (!text.is_empty()).then_some(text)
}

fn parent_of(path: &Path) -> PathBuf {
    path.parent().map_or_else(|| path.to_path_buf(), Path::to_path_buf)
}

pub fn refuse_if_too_wide(root: &Path) -> Result<(), String> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
    let is_home = home.as_deref().is_some_and(|home| clean(home) == root || clean(home).starts_with(root));
    let is_drive = root.parent().is_none() || root.components().filter(|part| matches!(part, Component::Normal(_))).count() == 0;
    if is_drive || is_home {
        return Err(format!(
            "root {} is too wide to index: name a repository, or a directory that holds a few of them",
            root.display()
        ));
    }
    Ok(())
}

#[must_use]
pub fn clean(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

#[must_use]
pub fn shown(path: &Path) -> String {
    clean(path).to_string_lossy().replace('\\', "/")
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
}

#[must_use]
pub fn linked_worktrees(root: &Path) -> Vec<Worktree> {
    let Ok(entries) = std::fs::read_dir(root.join(".git").join("worktrees")) else {
        return Vec::new();
    };
    let mut found: Vec<Worktree> = entries
        .flatten()
        .filter_map(|entry| {
            let pointer = std::fs::read_to_string(entry.path().join("gitdir")).ok()?;
            let path = clean(&parent_of(Path::new(pointer.trim())));
            if !path.is_dir() {
                return None;
            }
            let branch = std::fs::read_to_string(entry.path().join("HEAD"))
                .ok()
                .and_then(|head| head.trim().strip_prefix("ref: refs/heads/").map(str::to_owned));
            Some(Worktree { path, branch })
        })
        .collect();
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}
