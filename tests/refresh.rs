use omega::index::Index;
use omega::search::{Options, search};
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omega-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory can be created");
    dir
}

fn first_path(index: &Index, query: &str) -> Option<String> {
    let hit = search(index, query, &Options::default())
        .into_iter()
        .next()?;
    Some(
        index.files[index.chunks[hit.chunk].file as usize]
            .path
            .clone(),
    )
}

#[test]
fn added_changed_and_removed_files_are_followed() {
    let root = scratch("refresh");
    std::fs::write(
        root.join("alpha.rs"),
        "fn parse_invoice() {\n    todo!()\n}\n",
    )
    .unwrap();
    let index = Index::build(&root, None).unwrap();
    assert_eq!(
        first_path(&index, "parse_invoice").as_deref(),
        Some("alpha.rs")
    );
    assert_eq!(first_path(&index, "render_receipt"), None);

    std::fs::write(
        root.join("beta.rs"),
        "fn render_receipt() {\n    todo!()\n}\n",
    )
    .unwrap();
    let index = index.refreshed();
    assert_eq!(
        first_path(&index, "render_receipt").as_deref(),
        Some("beta.rs")
    );
    assert_eq!(
        first_path(&index, "parse_invoice").as_deref(),
        Some("alpha.rs")
    );

    std::fs::write(
        root.join("alpha.rs"),
        "fn settle_ledger_balance() {\n    todo!()\n}\n",
    )
    .unwrap();
    let index = index.refreshed();
    assert_eq!(
        first_path(&index, "settle_ledger_balance").as_deref(),
        Some("alpha.rs")
    );
    assert_eq!(first_path(&index, "parse_invoice"), None);

    std::fs::remove_file(root.join("beta.rs")).unwrap();
    let index = index.refreshed();
    assert_eq!(first_path(&index, "render_receipt"), None);
    assert_eq!(index.files.len(), 1);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn an_untouched_tree_with_a_skipped_file_is_not_rebuilt() {
    let root = scratch("skipped");
    std::fs::write(root.join("alpha.rs"), "fn parse_invoice() {}\n").unwrap();
    std::fs::write(
        root.join("bundle.js"),
        format!("var a={};\n", "1+".repeat(4000)),
    )
    .unwrap();
    let index = Index::build(&root, None).unwrap();
    assert_eq!(index.files.len(), 1);
    let before = index.chunks.as_ptr();
    let index = index.refreshed();
    assert_eq!(
        before,
        index.chunks.as_ptr(),
        "nothing changed, so nothing was rebuilt"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_kept_cache_is_followed_and_a_broken_one_ignored() {
    let root = scratch("kept");
    let cache = scratch("kept-cache");
    std::fs::write(
        root.join("alpha.rs"),
        "fn parse_invoice() {
    todo!()
}
",
    )
    .unwrap();
    let index = Index::open_in(&root, None, Some(&cache)).unwrap();
    assert_eq!(
        first_path(&index, "parse_invoice").as_deref(),
        Some("alpha.rs")
    );
    let stored: Vec<_> = std::fs::read_dir(&cache).unwrap().flatten().collect();
    assert_eq!(stored.len(), 1);
    drop(index);

    std::fs::write(
        root.join("beta.rs"),
        "fn render_receipt() {
    todo!()
}
",
    )
    .unwrap();
    let index = Index::open_in(&root, None, Some(&cache)).unwrap();
    assert_eq!(
        first_path(&index, "parse_invoice").as_deref(),
        Some("alpha.rs")
    );
    assert_eq!(
        first_path(&index, "render_receipt").as_deref(),
        Some("beta.rs")
    );
    drop(index);

    std::fs::write(stored[0].path(), b"not a cache").unwrap();
    let index = Index::open_in(&root, None, Some(&cache)).unwrap();
    assert_eq!(
        first_path(&index, "render_receipt").as_deref(),
        Some("beta.rs")
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn an_outline_lists_declarations_and_a_directory_lists_files() {
    let root = scratch("outline");
    std::fs::create_dir_all(root.join("src/api")).unwrap();
    std::fs::write(
        root.join("src/api/session.ts"),
        "export class Session {\n  private allowed: string[] = [];\n\n  async validateToken(token: string): Promise<void> {\n    if (bad) {\n      throw new Error();\n    }\n  }\n}\n\nexport function makeSession() {\n  return new Session();\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/main.ts"),
        "export const start = () => {\n  run();\n};\n",
    )
    .unwrap();
    let index = Index::build(&root, None).unwrap();

    let file = omega::outline::outline(&index, "session.ts");
    assert!(file.starts_with("src/api/session.ts  (13 lines)"), "{file}");
    assert!(file.contains("     1  export class Session"), "{file}");
    assert!(
        file.contains("     4    async validateToken(token: string): Promise<void>"),
        "{file}"
    );
    assert!(
        file.contains("    11  export function makeSession()"),
        "{file}"
    );
    assert!(!file.contains("     5"), "{file}");

    let directory = omega::outline::outline(&index, "src");
    assert!(
        directory.contains("api/  (1 files)") && directory.contains("main.ts  (3 lines)  start"),
        "{directory}"
    );
    assert!(omega::outline::outline(&index, "nowhere.rs").starts_with("No indexed file"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_regular_expression_and_a_quoted_name_are_found_in_first_party_files_only() {
    let root = scratch("grep");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("node_modules/lib")).unwrap();
    std::fs::write(
        root.join("src/cart.ts"),
        "export function stop() {\n  return call(\"cart.stop\", {});\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("node_modules/lib/index.js"),
        "function stop() {\n  return call(\"cart.stop\", {});\n}\n",
    )
    .unwrap();
    let index = Index::build(&root, None).unwrap();
    let options = omega::usages::Options::default();

    let matched = omega::usages::grep(&index, r#"call\("cart\.\w+""#, &options);
    assert!(matched.contains("1 line in 1 file") && matched.contains("[stop]"), "{matched}");
    assert!(!matched.contains("node_modules"), "{matched}");
    assert!(omega::usages::grep(&index, "(", &options).contains("not a regular expression"));

    let named = omega::usages::usages(&index, "cart.stop", &options);
    assert!(named.contains("`cart.stop`: 1 line") && named.contains("Found as text"), "{named}");
    let method = omega::usages::usages(&index, "cart.missing", &options);
    assert!(method.contains("not written in quotes"), "{method}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_wrong_path_is_answered_with_near_names_not_the_whole_directory() {
    let root = scratch("near");
    std::fs::create_dir_all(root.join("src/auth")).unwrap();
    for name in ["session.go", "sessions_store.go", "token.go", "guard.go"] {
        std::fs::write(root.join("src/auth").join(name), "func Keep() {\n\treturn\n}\n").unwrap();
    }
    let index = Index::build(&root, None).unwrap();

    let missed = omega::outline::outline(&index, "src/auth/sesion.go");
    assert!(missed.contains("Closest names:\n  src/auth/session.go"), "{missed}");
    assert!(missed.contains("`src/auth` exists and holds 4 files"), "{missed}");
    assert!(!missed.contains("guard.go") && !missed.contains("Keep"), "{missed}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn nothing_under_a_path_is_not_nothing_anywhere() {
    let root = scratch("elsewhere");
    std::fs::create_dir_all(root.join("api")).unwrap();
    std::fs::create_dir_all(root.join("web")).unwrap();
    std::fs::write(root.join("api/token.go"), "func ValidateToken() {\n\treturn\n}\n").unwrap();
    std::fs::write(root.join("web/cart.ts"), "export function clearCart() {\n  return 1;\n}\n").unwrap();
    let index = Index::build(&root, None).unwrap();

    let within = omega::usages::Options { path: Some("web".to_owned()), ..Default::default() };
    let answer = omega::usages::usages(&index, "ValidateToken", &within);
    assert!(answer.contains("under `web`") && answer.contains("api/token.go"), "{answer}");
    let answer = omega::usages::grep(&index, r"func \w+Token", &within);
    assert!(answer.contains("under `web`") && answer.contains("api/token.go"), "{answer}");
    let nowhere = omega::usages::Options { path: Some("mobile".to_owned()), ..Default::default() };
    assert!(omega::usages::usages(&index, "ValidateToken", &nowhere).contains("No indexed file has `mobile`"));

    let options = Options { path: Some("web".to_owned()), ..Options::default() };
    let hits = search(&index, "ValidateToken", &options);
    let answer = omega::search::render(&index, "ValidateToken", &hits, &options);
    assert!(answer.contains("No matches under `web`") && answer.contains("api/token.go"), "{answer}");

    let hits = search(&index, "ClearToken", &Options::default());
    let answer = omega::search::render(&index, "ClearToken", &hits, &Options::default());
    assert!(answer.contains("Nothing in the code is named `ClearToken`") && !answer.contains("token.go"), "{answer}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_document_is_outlined_by_its_headings_and_found_by_them() {
    let root = scratch("docs");
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::write(
        root.join("docs/T07-cart-limits.md"),
        "# T07 — Cart limits and quotas\n\nWhy.\n\n## Method and limits\n\nA cart holds at most forty lines.\n\n```rs\nfn not_a_heading() {}\n# not a heading either\n```\n\n## Done when\n\nTests pass.\n",
    )
    .unwrap();
    std::fs::write(root.join("cart.rs"), "fn limits() {\n    // method and limits are elsewhere\n}\n").unwrap();
    let index = Index::build(&root, None).unwrap();

    let outline = omega::outline::outline(&index, "docs/T07-cart-limits.md");
    assert!(outline.contains("1  # T07 — Cart limits and quotas") && outline.contains("5  ## Method and limits"), "{outline}");
    assert!(!outline.contains("not_a_heading") && !outline.contains("not a heading"), "{outline}");
    let listing = omega::outline::outline(&index, "docs");
    assert!(listing.contains("T07-cart-limits.md") && listing.contains("T07 — Cart limits and quotas") && !listing.contains("Done when"), "{listing}");

    let options = Options::default();
    for query in ["method and limits", "T07"] {
        let hits = search(&index, query, &options);
        let answer = omega::search::render(&index, query, &hits, &options);
        assert!(answer.contains("A document has a section by that name") && answer.contains("T07-cart-limits.md"), "{query}: {answer}");
    }
    let found = omega::usages::usages(&index, "at most forty lines", &omega::usages::Options::default());
    assert!(found.contains("T07-cart-limits.md"), "{found}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_store_grows_by_segments_and_is_merged_into_one() {
    let root = scratch("segments");
    let cache = scratch("segments-cache");
    let records = || {
        let stored: Vec<_> = std::fs::read_dir(&cache).unwrap().flatten().collect();
        assert_eq!(stored.len(), 1, "one file per repository");
        let bytes = std::fs::read(stored[0].path()).unwrap();
        bytes.windows(8).filter(|window| window == b"OMEGAIDX").count()
    };
    let names = ["parse_invoice", "render_receipt", "settle_ledger", "audit_trail"];
    for (round, name) in names.iter().enumerate() {
        std::fs::write(root.join(format!("f{round}.rs")), format!("fn handler_{round}() {{\n    todo!()\n}}\n")).unwrap();
        std::fs::write(root.join("moving.rs"), format!("fn {name}() {{\n    todo!()\n}}\n")).unwrap();
        let (index, _) = Index::open_lexical_in(&root, None, Some(&cache), &|_| {}).unwrap();
        assert_eq!(first_path(&index, name).as_deref(), Some("moving.rs"));
        assert_eq!(index.files.len(), round + 2);
    }
    assert_eq!(records(), 4);

    let (index, upkeep) = Index::open_lexical_in(&root, None, Some(&cache), &|_| {}).unwrap();
    upkeep.tidy().run(&|_| {}).unwrap();
    drop(index);
    assert_eq!(records(), 1);
    let (index, upkeep) = Index::open_lexical_in(&root, None, Some(&cache), &|_| {}).unwrap();
    assert!(upkeep.is_idle());
    assert_eq!(first_path(&index, "handler_0").as_deref(), Some("f0.rs"));
    assert_eq!(first_path(&index, "audit_trail").as_deref(), Some("moving.rs"));
    assert_eq!(first_path(&index, "render_receipt"), None);

    drop(index);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&cache);
}

fn tables(index: &Index) -> Vec<(String, u32, u32, Vec<String>)> {
    index
        .chunks
        .iter()
        .map(|chunk| {
            let path = index.files[chunk.file as usize].path.clone();
            (path, chunk.start_line, chunk.end_line, chunk.names.clone())
        })
        .collect()
}

#[test]
fn an_index_kept_up_to_date_holds_what_one_built_afresh_does() {
    let root = scratch("merged-tables");
    let cache = scratch("merged-tables-cache");
    let source = |name: &str| format!("fn {name}() {{\n    todo!()\n}}\n\nfn {name}_helper() {{\n    todo!()\n}}\n");
    for at in 0..40 {
        std::fs::write(root.join(format!("m{at:03}.rs")), source(&format!("start_{at}"))).unwrap();
    }
    let mut index = Index::open_in(&root, None, Some(&cache)).unwrap();
    let rounds: [&dyn Fn(&std::path::Path); 5] = [
        &|root| {
            std::fs::write(root.join("m005.rs"), source("changed_five")).unwrap();
            std::fs::write(root.join("m005a.rs"), source("added_between")).unwrap();
            std::fs::remove_file(root.join("m010.rs")).unwrap();
        },
        &|root| {
            std::fs::write(root.join("m005.rs"), source("changed_five_again")).unwrap();
            std::fs::remove_file(root.join("m005a.rs")).unwrap();
            std::fs::write(root.join("a_first.rs"), source("added_first")).unwrap();
        },
        &|root| {
            for at in 0..300 {
                std::fs::write(root.join(format!("z{at:03}.rs")), source(&format!("bulk_{at}"))).unwrap();
            }
        },
        &|root| {
            std::fs::write(root.join("z100.rs"), source("bulk_changed")).unwrap();
            std::fs::remove_file(root.join("z200.rs")).unwrap();
            std::fs::write(root.join("m020.rs"), "").unwrap();
        },
        &|root| std::fs::remove_file(root.join("m020.rs")).unwrap(),
    ];
    for round in rounds {
        round(&root);
        index = index.refreshed();
        let afresh = Index::build(&root, None).unwrap();
        assert_eq!(tables(&index), tables(&afresh));
        for query in ["changed_five_again", "added_first", "bulk_changed", "bulk_7", "start_3"] {
            assert_eq!(first_path(&index, query), first_path(&afresh, query), "{query}");
        }
    }
    index.persist();
    drop(index);
    let reopened = Index::open_in(&root, None, Some(&cache)).unwrap();
    assert_eq!(tables(&reopened), tables(&Index::build(&root, None).unwrap()));

    drop(reopened);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&cache);
}

#[test]
fn a_worktree_reads_only_the_files_its_main_checkout_does_not_hold_as_they_are() {
    let ws = scratch("borrowed");
    let cache = scratch("borrowed-cache");
    let (main, fix) = (ws.join("app"), ws.join("app-fix"));
    for dir in [main.join(".git/worktrees/fix"), main.join("src"), fix.join("src")] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(fix.join(".git"), format!("gitdir: {}\n", main.join(".git/worktrees/fix").display())).unwrap();
    std::fs::write(main.join(".git/worktrees/fix/commondir"), "../..\n").unwrap();
    std::fs::write(main.join(".git/worktrees/fix/gitdir"), format!("{}\n", fix.join(".git").display())).unwrap();
    for at in 0..30 {
        let text = format!("fn shared_{at}() {{\n    todo!()\n}}\n");
        std::fs::write(main.join(format!("src/m{at:02}.rs")), &text).unwrap();
        std::fs::write(fix.join(format!("src/m{at:02}.rs")), &text).unwrap();
    }
    std::fs::write(fix.join("src/m07.rs"), "fn changed_in_fix() {\n    todo!()\n}\n").unwrap();
    std::fs::write(fix.join("src/only_fix.rs"), "fn only_in_fix() {\n    todo!()\n}\n").unwrap();
    drop(Index::open_in(&main, None, Some(&cache)).unwrap());

    let read = std::sync::Mutex::new(Vec::new());
    let (index, _) = Index::open_lexical_in(&fix, None, Some(&cache), &|progress| {
        if let omega::index::Progress::Reading { total, .. } = progress {
            read.lock().unwrap().push(total);
        }
    })
    .unwrap();
    assert_eq!(read.lock().unwrap().first().copied(), Some(2), "only the changed and the new file are read");
    assert_eq!(first_path(&index, "changed_in_fix").as_deref(), Some("src/m07.rs"));
    assert_eq!(first_path(&index, "shared_3").as_deref(), Some("src/m03.rs"));
    assert_eq!(tables(&index), tables(&Index::build(&fix, None).unwrap()));
    drop(index);
    let (again, _) = Index::open_lexical_in(&fix, None, Some(&cache), &|progress| {
        assert!(!matches!(progress, omega::index::Progress::Reading { .. }), "a second start reads nothing");
    })
    .unwrap();
    assert_eq!(tables(&again), tables(&Index::build(&fix, None).unwrap()));
    drop(again);
    let _ = std::fs::remove_dir_all(&ws);
    let _ = std::fs::remove_dir_all(&cache);
}
