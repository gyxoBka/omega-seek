//! An index follows the tree it was built from.

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

    // Added.
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

    // Changed: a different size, so the stamp differs whatever the clock says.
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

    // Removed.
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
    // One enormous line: minified, so looked at and left out.
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

    // A second start answers the same from the cache, and sees what changed since.
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

    // A cache that is not one is a slower start, not a failure.
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
    // `if (bad) {` opens a block and declares nothing.
    assert!(!file.contains("     5"), "{file}");

    let directory = omega::outline::outline(&index, "src");
    assert!(
        directory.contains("api/  (1 files)") && directory.contains("main.ts  (3 lines)  start"),
        "{directory}"
    );
    assert!(omega::outline::outline(&index, "nowhere.rs").starts_with("No indexed file"));
    let _ = std::fs::remove_dir_all(&root);
}
