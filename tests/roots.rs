use omega::roots::{Resolved, clean, linked_worktrees, resolve};
use std::path::{Path, PathBuf};

fn workspace(name: &str) -> PathBuf {
    let ws = std::env::temp_dir().join(format!("omega-roots-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ws);
    for dir in [
        "app/.git/worktrees/fix",
        "app/src/api",
        "app/.agents/worktrees/fix/src",
        "lib/.git",
        "lib/src",
    ] {
        std::fs::create_dir_all(ws.join(dir)).unwrap();
    }
    std::fs::write(
        ws.join("app/src/api/guard.ts"),
        "export function check() {}\n",
    )
    .unwrap();
    std::fs::write(ws.join("lib/src/parse.rs"), "fn parse() {}\n").unwrap();
    let worktree = ws.join("app/.agents/worktrees/fix");
    std::fs::write(
        worktree.join(".git"),
        "gitdir: ../../../.git/worktrees/fix\n",
    )
    .unwrap();
    std::fs::write(
        worktree.join("src/only_here.ts"),
        "export function onlyHere() {}\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("app/.git/worktrees/fix/gitdir"),
        format!("{}\n", worktree.join(".git").display()),
    )
    .unwrap();
    std::fs::write(
        ws.join("app/.git/worktrees/fix/HEAD"),
        "ref: refs/heads/fix-auth\n",
    )
    .unwrap();
    clean(&ws.canonicalize().unwrap())
}

fn at(root: &Path, within: Option<&str>, elsewhere: bool) -> Resolved {
    Resolved {
        root: root.to_path_buf(),
        within: within.map(str::to_owned),
        elsewhere,
    }
}

#[test]
fn a_call_without_a_root_is_about_home() {
    let ws = workspace("home");
    let home = ws.join("app");
    assert_eq!(resolve(&home, None, None).unwrap(), at(&home, None, false));
    assert_eq!(
        resolve(&home, None, Some("src/api")).unwrap(),
        at(&home, Some("src/api"), false)
    );
    assert_eq!(
        resolve(&home, Some("  "), None).unwrap(),
        at(&home, None, false)
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_root_names_a_sibling_a_parent_or_a_directory_inside_a_checkout() {
    let ws = workspace("named");
    let home = ws.join("app");
    assert_eq!(
        resolve(&home, Some("../lib"), None).unwrap(),
        at(&ws.join("lib"), None, true)
    );
    let absolute = ws.join("lib").to_string_lossy().into_owned();
    assert_eq!(
        resolve(&home, Some(&absolute), None).unwrap(),
        at(&ws.join("lib"), None, true)
    );
    assert_eq!(
        resolve(&home, Some(".."), None).unwrap(),
        at(&ws, None, true)
    );
    assert_eq!(
        resolve(&home, Some("../lib/src"), None).unwrap(),
        at(&ws.join("lib"), Some("src"), true)
    );
    assert_eq!(
        resolve(&home, Some("../lib/src"), Some("parse.rs")).unwrap(),
        at(&ws.join("lib"), Some("src/parse.rs"), true)
    );
    assert_eq!(
        resolve(&home, Some("."), None).unwrap(),
        at(&home, None, false)
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn an_absolute_path_implies_its_checkout() {
    let ws = workspace("implied");
    let home = ws.join("app");
    let text = |path: PathBuf| path.to_string_lossy().into_owned();
    let inside = text(home.join("src/api/guard.ts"));
    assert_eq!(
        resolve(&home, None, Some(&inside)).unwrap(),
        at(&home, Some("src/api/guard.ts"), false)
    );
    let worktree = home.join(".agents/worktrees/fix");
    let in_worktree = text(worktree.join("src/only_here.ts"));
    assert_eq!(
        resolve(&home, None, Some(&in_worktree)).unwrap(),
        at(&worktree, Some("src/only_here.ts"), true)
    );
    let planned = text(worktree.join("src/new/handler.ts"));
    assert_eq!(
        resolve(&home, None, Some(&planned)).unwrap(),
        at(&worktree, Some("src/new/handler.ts"), true)
    );
    let next_door = text(ws.join("lib/src/parse.rs"));
    assert_eq!(
        resolve(&home, None, Some(&next_door)).unwrap(),
        at(&ws.join("lib"), Some("src/parse.rs"), true)
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn a_parent_started_in_keeps_its_checkouts_as_one_tree() {
    let ws = workspace("parent");
    let path = ws.join("lib/src/parse.rs").to_string_lossy().into_owned();
    assert_eq!(
        resolve(&ws, None, Some(&path)).unwrap(),
        at(&ws, Some("lib/src/parse.rs"), false)
    );
    let worktree = ws.join("app/.agents/worktrees/fix");
    let hidden = worktree
        .join("src/only_here.ts")
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        resolve(&ws, None, Some(&hidden)).unwrap(),
        at(&worktree, Some("src/only_here.ts"), true)
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn what_cannot_be_a_root_is_refused_with_a_reason() {
    let ws = workspace("refused");
    let home = ws.join("app");
    assert!(
        resolve(&home, Some("../nowhere"), None)
            .unwrap_err()
            .contains("does not exist")
    );
    let drive = if cfg!(windows) { "C:\\" } else { "/" };
    assert!(
        resolve(&home, Some(drive), None)
            .unwrap_err()
            .contains("too wide")
    );
    if let Some(user) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        let user = PathBuf::from(user);
        if !user.join(".git").exists() && user.is_dir() {
            assert!(
                resolve(&home, Some(&user.to_string_lossy()), None)
                    .unwrap_err()
                    .contains("too wide")
            );
        }
    }
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn linked_worktrees_are_read_from_git_itself() {
    let ws = workspace("linked");
    let found = linked_worktrees(&ws.join("app"));
    assert_eq!(found.len(), 1);
    assert_eq!(
        clean(&found[0].path.canonicalize().unwrap()),
        ws.join("app/.agents/worktrees/fix")
    );
    assert_eq!(found[0].branch.as_deref(), Some("fix-auth"));
    assert!(linked_worktrees(&ws.join("lib")).is_empty());
    assert!(linked_worktrees(&ws.join("app/.agents/worktrees/fix")).is_empty());
    let _ = std::fs::remove_dir_all(&ws);
}
