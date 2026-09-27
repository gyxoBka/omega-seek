use omega::index::Index;
use omega::roots::clean;
use omega::search::{Options, search};
use omega::watch::{Changes, Watcher};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omega-watch-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    clean(&dir.canonicalize().unwrap())
}

fn first_path(index: &Index, query: &str) -> Option<String> {
    let hit = search(index, query, &Options::default()).into_iter().next()?;
    Some(index.files[index.chunks[hit.chunk].file as usize].path.clone())
}

fn tables(index: &Index) -> Vec<(String, u32, u32, Vec<String>)> {
    index
        .chunks
        .iter()
        .map(|chunk| (index.files[chunk.file as usize].path.clone(), chunk.start_line, chunk.end_line, chunk.names.clone()))
        .collect()
}

fn source(name: &str) -> String {
    format!("fn {name}() {{\n    todo!()\n}}\n")
}

fn caught_up(watcher: &Watcher, index: Index) -> (Index, bool) {
    match watcher.sync(Duration::from_secs(2)).expect("the cookie came back") {
        Changes::Paths(paths) => (index.refreshed_with(&paths), false),
        Changes::Everything => (index.refreshed(), true),
    }
}

#[test]
fn an_edit_made_just_before_a_call_is_always_seen() {
    let root = scratch("edits");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/start.rs"), source("start_here")).unwrap();
    let watcher = Watcher::start(&root).expect("a watcher on a local directory");
    let mut index = Index::build(&root, None).unwrap();
    let words = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"];
    for round in 0..120 {
        let name = format!("edited_{}_{round}", words[round % words.len()]);
        std::fs::write(root.join(format!("src/f{}.rs", round % 5)), source(&name)).unwrap();
        index = caught_up(&watcher, index).0;
        assert_eq!(first_path(&index, &name).as_deref(), Some(format!("src/f{}.rs", round % 5).as_str()), "round {round}");
    }
    assert_eq!(tables(&index), tables(&Index::build(&root, None).unwrap()));
    drop(watcher);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn directories_made_moved_and_removed_are_followed_as_a_walk_would() {
    let root = scratch("dirs");
    std::fs::write(root.join(".gitignore"), "build/\n").unwrap();
    std::fs::write(root.join("keep.rs"), source("kept_here")).unwrap();
    let watcher = Watcher::start(&root).unwrap();
    let mut index = Index::build(&root, None).unwrap();
    let steps: [&dyn Fn(&Path); 6] = [
        &|root| {
            std::fs::create_dir_all(root.join("api/v1")).unwrap();
            std::fs::write(root.join("api/v1/orders.rs"), source("list_orders")).unwrap();
            std::fs::write(root.join("api/users.rs"), source("list_users")).unwrap();
        },
        &|root| std::fs::rename(root.join("api"), root.join("service")).unwrap(),
        &|root| {
            std::fs::create_dir_all(root.join("build")).unwrap();
            std::fs::write(root.join("build/generated.rs"), source("generated_code")).unwrap();
            std::fs::create_dir_all(root.join(".hidden")).unwrap();
            std::fs::write(root.join(".hidden/secret.rs"), source("hidden_code")).unwrap();
        },
        &|root| std::fs::rename(root.join("keep.rs"), root.join("service/keep.rs")).unwrap(),
        &|root| std::fs::remove_dir_all(root.join("service/v1")).unwrap(),
        &|root| std::fs::remove_dir_all(root.join("service")).unwrap(),
    ];
    for (at, step) in steps.iter().enumerate() {
        step(&root);
        index = caught_up(&watcher, index).0;
        assert_eq!(tables(&index), tables(&Index::build(&root, None).unwrap()), "step {at}");
    }
    assert_eq!(first_path(&index, "generated_code"), None, "ignored stays ignored");
    drop(watcher);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_change_to_what_is_ignored_walks_the_tree() {
    let root = scratch("ignored");
    std::fs::create_dir_all(root.join("gen")).unwrap();
    std::fs::write(root.join("gen/out.rs"), source("generated_out")).unwrap();
    std::fs::write(root.join("main.rs"), source("main_code")).unwrap();
    let watcher = Watcher::start(&root).unwrap();
    let index = Index::build(&root, None).unwrap();
    assert!(first_path(&index, "generated_out").is_some());
    std::fs::write(root.join(".gitignore"), "gen/\n").unwrap();
    let (index, walked) = caught_up(&watcher, index);
    assert!(walked, "a new ignore rule is not a changed path");
    assert_eq!(first_path(&index, "generated_out"), None);
    let (_, walked) = caught_up(&watcher, index);
    assert!(!walked, "and only once");
    drop(watcher);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn nothing_changed_is_nothing_to_do_and_the_cookie_is_gone() {
    let root = scratch("quiet");
    std::fs::write(root.join("main.rs"), source("main_code")).unwrap();
    let watcher = Watcher::start(&root).unwrap();
    let _ = watcher.sync(Duration::from_secs(2));
    match watcher.sync(Duration::from_secs(2)) {
        Some(Changes::Paths(paths)) => assert!(paths.is_empty(), "{paths:?}"),
        other => panic!("{other:?}"),
    }
    let left: Vec<String> = std::fs::read_dir(&root).unwrap().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left, ["main.rs"]);
    drop(watcher);
    let _ = std::fs::remove_dir_all(&root);
}
