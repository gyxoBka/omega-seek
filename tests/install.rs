//! Install puts in an entry, a block and a file; uninstall takes out exactly those.

use omega::install::agents::{Dirs, agents};
use omega::install::config::Action;
use omega::install::{Integration, Mode, apply};
use serde_json::Value;
use std::path::{Path, PathBuf};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("omega-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn put(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

const EXE: &str = r"C:\Tools\omega.exe";

#[test]
fn claude_round_trip_leaves_what_was_there() {
    let home = scratch("claude");
    let claude_json = "{\n  \"numStartups\": 7,\n  \"mcpServers\": {\n    \"other\": {\n      \"command\": \"x\"\n    }\n  },\n  \"theme\": \"dark\"\n}\n";
    let claude_md = "# Mine\n\nKeep this.\n";
    put(&home.join(".claude.json"), claude_json);
    put(&home.join(".claude/CLAUDE.md"), claude_md);
    let all = agents(&Dirs::under(&home));
    let claude = all.iter().find(|agent| agent.id == "claude").unwrap();

    for integration in Integration::ALL {
        let action = apply(Mode::Install, claude, integration, Path::new(EXE));
        assert!(
            matches!(action, Action::Created | Action::Updated),
            "{integration:?}: {action:?}"
        );
    }
    let written: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(".claude.json")).unwrap()).unwrap();
    assert_eq!(written["mcpServers"]["omega"]["command"], EXE);
    assert_eq!(written["mcpServers"]["omega"]["args"][0], "mcp");
    assert_eq!(written["mcpServers"]["other"]["command"], "x");
    // Key order survives: nothing is shuffled in somebody else's file.
    let keys: Vec<&String> = written.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["numStartups", "mcpServers", "theme"]);
    let md = std::fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap();
    assert!(
        md.starts_with(claude_md.trim_end()) && md.contains("OMEGA_START") && md.contains("usages")
    );
    assert!(home.join(".claude/agents/omega.md").exists());
    assert!(home.join(".claude.json.omega.bak").exists());

    // Again: nothing to do.
    for integration in Integration::ALL {
        assert_eq!(
            apply(Mode::Install, claude, integration, Path::new(EXE)),
            Action::Unchanged
        );
    }

    for integration in Integration::ALL {
        assert_eq!(
            apply(Mode::Uninstall, claude, integration, Path::new(EXE)),
            Action::Removed
        );
    }
    assert_eq!(
        std::fs::read_to_string(home.join(".claude.json")).unwrap(),
        claude_json
    );
    assert_eq!(
        std::fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap(),
        claude_md
    );
    assert!(!home.join(".claude/agents/omega.md").exists());
    assert!(!home.join(".claude.json.omega.bak").exists());
    for integration in Integration::ALL {
        assert_eq!(
            apply(Mode::Uninstall, claude, integration, Path::new(EXE)),
            Action::NotFound
        );
    }
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn codex_toml_keeps_its_other_tables_and_comments() {
    let home = scratch("codex");
    let config = "# my settings\nmodel = \"o\"\n\n[mcp_servers.other]\ncommand = \"x\"\n";
    put(&home.join(".codex/config.toml"), config);
    let all = agents(&Dirs::under(&home));
    let codex = all.iter().find(|agent| agent.id == "codex").unwrap();

    assert_eq!(
        apply(Mode::Install, codex, Integration::Mcp, Path::new(EXE)),
        Action::Updated
    );
    let written = std::fs::read_to_string(home.join(".codex/config.toml")).unwrap();
    assert!(written.starts_with(config.trim_end()));
    assert!(
        written.contains("[mcp_servers.omega]\ncommand = 'C:\\Tools\\omega.exe'\nargs = [\"mcp\"]")
    );
    assert_eq!(
        apply(Mode::Install, codex, Integration::Mcp, Path::new(EXE)),
        Action::Unchanged
    );

    assert_eq!(
        apply(Mode::Uninstall, codex, Integration::Mcp, Path::new(EXE)),
        Action::Removed
    );
    assert_eq!(
        std::fs::read_to_string(home.join(".codex/config.toml")).unwrap(),
        config
    );
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_config_with_comments_is_left_alone() {
    let home = scratch("vscode");
    let config = "{\n  // my servers\n  \"servers\": {}\n}\n";
    let all = agents(&Dirs::under(&home));
    let vscode = all.iter().find(|agent| agent.id == "vscode").unwrap();
    let path = vscode.mcp.as_ref().unwrap().0.clone();
    put(&path, config);

    assert!(matches!(
        apply(Mode::Install, vscode, Integration::Mcp, Path::new(EXE)),
        Action::Skipped(_)
    ));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), config);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn a_fresh_home_gets_new_files_and_loses_them_again() {
    let home = scratch("fresh");
    let all = agents(&Dirs::under(&home));
    let opencode = all.iter().find(|agent| agent.id == "opencode").unwrap();
    for integration in Integration::ALL {
        assert_eq!(
            apply(Mode::Install, opencode, integration, Path::new(EXE)),
            Action::Created
        );
    }
    let path = &opencode.mcp.as_ref().unwrap().0;
    let written: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(written["mcp"]["omega"]["command"][1], "mcp");
    assert_eq!(written["mcp"]["omega"]["type"], "local");
    for integration in [Integration::Instructions, Integration::Subagent] {
        assert_eq!(
            apply(Mode::Uninstall, opencode, integration, Path::new(EXE)),
            Action::Removed
        );
    }
    assert_eq!(
        apply(Mode::Uninstall, opencode, Integration::Mcp, Path::new(EXE)),
        Action::Removed
    );
    assert!(
        !path.exists(),
        "a config install created holds nothing else, so it is gone"
    );
    // The instructions file held nothing but our block, so it is gone.
    assert!(!opencode.instructions.as_ref().unwrap().exists());
    let _ = std::fs::remove_dir_all(&home);
}

/// Counting what install writes, not just reading its verdict: running it
/// again -- from the same place or after the binary moved -- leaves one entry,
/// one block and one table, never two.
#[test]
fn installing_again_never_writes_anything_twice() {
    let home = scratch("twice");
    put(&home.join(".claude.json"), "{\n  \"mcpServers\": {}\n}\n");
    put(
        &home.join(".claude/CLAUDE.md"),
        "# Mine\r\n\r\nKeep this.\r\n",
    );
    put(&home.join(".codex/config.toml"), "model = \"o\"\n");
    put(&home.join(".codex/AGENTS.md"), "# Codex notes\n");
    let all = agents(&Dirs::under(&home));
    let chosen: Vec<_> = all
        .iter()
        .filter(|agent| ["claude", "codex"].contains(&agent.id))
        .collect();

    let snapshot = |home: &Path| -> Vec<String> {
        [
            ".claude.json",
            ".claude/CLAUDE.md",
            ".claude/agents/omega.md",
            ".codex/config.toml",
            ".codex/AGENTS.md",
            ".codex/agents/omega.toml",
        ]
        .iter()
        .map(|name| std::fs::read_to_string(home.join(name)).unwrap())
        .collect()
    };
    let install = |exe: &str| {
        for agent in &chosen {
            for integration in Integration::ALL {
                let action = apply(Mode::Install, agent, integration, Path::new(exe));
                assert!(
                    !matches!(action, Action::Failed(_) | Action::Skipped(_)),
                    "{action:?}"
                );
            }
        }
    };

    install(EXE);
    let once = snapshot(&home);
    install(EXE);
    install(EXE);
    assert_eq!(
        snapshot(&home),
        once,
        "a second and third install changed nothing"
    );

    // The binary moved: every reference follows it, and there is still one of each.
    let moved = r"D:\Elsewhere\omega.exe";
    install(moved);
    let after = snapshot(&home);
    for (text, name) in after.iter().zip([
        "json",
        "CLAUDE.md",
        "agent",
        "toml",
        "AGENTS.md",
        "codex agent",
    ]) {
        assert!(
            !text.contains(r"C:\Tools") && !text.contains(r"C:\\Tools"),
            "{name} still names the old path"
        );
    }
    assert_eq!(after[0].matches("\"omega\"").count(), 1);
    assert_eq!(after[1].matches("OMEGA_START").count(), 1);
    assert_eq!(after[1].matches("OMEGA_END").count(), 1);
    assert!(
        after[1].starts_with("# Mine\r\n\r\nKeep this."),
        "the user's own lines are as they were"
    );
    assert_eq!(after[3].matches("[mcp_servers.omega]").count(), 1);
    assert_eq!(after[4].matches("OMEGA_START").count(), 1);
    assert!(after[4].starts_with("# Codex notes"));
    let _ = std::fs::remove_dir_all(&home);
}

/// The block an agent reads on every session: it has to stay short, say when to
/// use which tool and when grep, say why (no junk in the answers), and show calls.
#[test]
fn the_instructions_are_short_and_say_what_they_must() {
    let home = scratch("wording");
    let all = agents(&Dirs::under(&home));
    let claude = all.iter().find(|agent| agent.id == "claude").unwrap();
    let written = apply(
        Mode::Install,
        claude,
        Integration::Instructions,
        Path::new(EXE),
    );
    assert_eq!(written, Action::Created);
    let block = std::fs::read_to_string(home.join(".claude/CLAUDE.md")).unwrap();

    let words = block.split_whitespace().count();
    assert!(words <= 340, "{words} words: every session pays for these");
    for needed in [
        "FIRST",
        "node_modules",
        "search(\"",
        "usages(\"",
        "outline(\"",
        "whole declaration",
        "Low confidence",
        "Grep/Glob only for regular expressions",
        "root=\"../backend\"",
        "git worktree",
        "omega search",
    ] {
        assert!(
            block.contains(needed),
            "the instructions no longer say `{needed}`"
        );
    }
    // The agent calls MCP tools and a shell finds the command on PATH: where
    // the binary lives is the MCP entry's business, not a cost of every session.
    assert!(
        !block.contains(EXE) && !block.contains("{exe}"),
        "the instructions name a path"
    );
    let _ = std::fs::remove_dir_all(&home);
}

/// Another tool's installer leaves `{"mcpServers": {}}` in directories it made
/// for agents nobody installed; that is not an agent to offer by default.
#[test]
fn a_real_config_or_a_filled_in_stub_is_an_installed_agent() {
    let home = scratch("stubs");
    put(
        &home.join(".kiro/settings/mcp.json"),
        "{\n  \"mcpServers\": {}\n}\n",
    );
    put(
        &home.join(".gemini/settings.json"),
        "{\n  \"mcpServers\": {}\n}\n",
    );
    put(
        &home.join(".gemini/antigravity/mcp_config.json"),
        "{ \"mcpServers\": {} }",
    );
    put(&home.join(".codex/config.toml"), "model = \"o\"\n");
    put(
        &home.join(".cursor/mcp.json"),
        "{\"mcpServers\": {\"other\": {\"command\": \"x\"}}}",
    );
    let all = agents(&Dirs::under(&home));
    let detected = |id: &str| all.iter().find(|agent| agent.id == id).unwrap().detected();
    // A real config, or a stub that somebody has since filled in, counts.
    assert!(detected("codex"));
    assert!(detected("cursor"));
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn hollow_directories_are_told_from_lived_in_ones() {
    let home = scratch("hollow");
    put(
        &home.join(".kiro/settings/mcp.json"),
        "{\n  \"mcpServers\": {}\n}\n",
    );
    let all = agents(&Dirs::under(&home));
    let kiro = all.iter().find(|agent| agent.id == "kiro").unwrap();
    if !kiro_on_path() {
        assert!(
            !kiro.detected(),
            "a directory of empty stubs was taken for an installed agent"
        );
        // Once it holds anything real, it is one.
        put(&home.join(".kiro/steering/notes.md"), "# mine\n");
        assert!(kiro.detected());
    }
    let _ = std::fs::remove_dir_all(&home);
}

fn kiro_on_path() -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| {
            ["kiro", "kiro.exe", "kiro.cmd"]
                .iter()
                .any(|name| dir.join(name).is_file())
        })
    })
}
