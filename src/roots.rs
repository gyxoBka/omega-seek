//! Which directory a call is about.
//!
//! A server is started in one repository, and an agent does not always stay in
//! it: it is sent into a git worktree, or asks about the repository next door.
//! A call may therefore name a `root`, or pass an absolute path that implies
//! one. Nothing is remembered between calls -- sub-agents can share one server
//! process, and one of them in a worktree must not redirect the others.

use std::path::{Component, Path, PathBuf};

/// Where a call looks, worked out from what it passed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resolved {
    pub root: PathBuf,
    /// What is left of a path that pointed inside the root, as a path filter.
    pub within: Option<String>,
    /// Whether this is somewhere other than where the server was started.
    pub elsewhere: bool,
}

/// `root` is absolute or relative to `home`, the directory the server indexes
/// by default. A directory inside a repository means that repository, narrowed
/// to the directory: the unit an index is built for is a checkout. `path` is
/// the call's path argument; when absolute it implies a root by itself.
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
        // A root narrowed to a directory and a path within it say one thing.
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
        // The nearest existing ancestor: the path may name a file yet to be written.
        let mut existing = clean(Path::new(path));
        while !existing.exists() {
            existing = parent_of(&existing);
        }
        let existing = clean(&existing.canonicalize().unwrap_or(existing));
        let directory = if existing.is_dir() { existing } else { parent_of(&existing) };
        let (nearest, _) = checkout_of(&directory);
        let checkout = nearest.join(".git").exists().then_some(nearest);
        let top = match checkout {
            // A checkout of its own inside the home root -- harnesses put
            // worktrees under a hidden directory of the repository itself, where
            // the home index does not look.
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

/// The git directory every checkout of a repository shares: the main
/// checkout's `.git`, which a linked worktree's `.git` file leads to through
/// its `commondir`. None for a directory that is not a checkout.
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
        // A submodule's git directory is its own.
        Err(_) => gitdir,
    };
    Some(clean(&common.canonicalize().ok()?))
}

/// The main checkout of the repository `directory` is in: what access is
/// given to, so that every worktree of a repository has the same. A directory
/// under no checkout stands for itself.
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

/// The checkout `directory` belongs to -- the nearest ancestor holding `.git`,
/// as a directory or as a worktree's file -- and where inside it `directory`
/// sits. A directory under no checkout, such as one holding several
/// repositories, is its own root.
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

/// Whether a checkout nested inside `home` is searched on its own rather than
/// as part of it: when `home` is itself a checkout, or the nested one sits
/// under a hidden directory, which the walk of `home` never enters.
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

/// A drive, a home directory, or the directory above one would be read whole.
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

/// Windows canonical paths come back as `\\?\C:\...`; nothing downstream wants that.
#[must_use]
pub fn clean(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => path.to_path_buf(),
    }
}

/// How a root is written in an answer: forward slashes, so that a path built
/// from it reads the same everywhere and needs no escaping.
#[must_use]
pub fn shown(path: &Path) -> String {
    clean(path).to_string_lossy().replace('\\', "/")
}

/// A checkout of the same repository elsewhere on disk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: Option<String>,
}

/// The linked worktrees of the repository whose main checkout is `root`. An
/// agent working in one of them, asking a server started in the main checkout,
/// would be answered from code it is not editing.
#[must_use]
pub fn linked_worktrees(root: &Path) -> Vec<Worktree> {
    let Ok(entries) = std::fs::read_dir(root.join(".git").join("worktrees")) else {
        return Vec::new();
    };
    let mut found: Vec<Worktree> = entries
        .flatten()
        .filter_map(|entry| {
            // `gitdir` holds the path of the worktree's own `.git` file.
            let pointer = std::fs::read_to_string(entry.path().join("gitdir")).ok()?;
            let path = clean(&parent_of(Path::new(pointer.trim())));
            if !path.is_dir() {
                return None; // removed by hand; git has not pruned it yet
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
