//! The coding agents omega can be installed into, and where each keeps
//! its MCP servers, its standing instructions and its sub-agents.

use std::path::{Path, PathBuf};

/// The directories agent configuration hangs off. `OMEGA_HOME` replaces
/// all of them, so an install can be rehearsed without touching a real one.
#[derive(Clone, Debug)]
pub struct Dirs {
    pub home: PathBuf,
    /// `%APPDATA%` on Windows, `~/Library/Application Support` on macOS,
    /// `$XDG_CONFIG_HOME` or `~/.config` elsewhere.
    pub app_config: PathBuf,
    /// `$XDG_CONFIG_HOME` or `~/.config`, on every platform.
    pub xdg_config: PathBuf,
}

impl Dirs {
    pub fn discover() -> Result<Self, String> {
        if let Some(home) = std::env::var_os("OMEGA_HOME") {
            return Ok(Self::under(Path::new(&home)));
        }
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .ok_or("no home directory")?;
        let xdg_config = std::env::var_os("XDG_CONFIG_HOME")
            .map_or_else(|| home.join(".config"), PathBuf::from);
        let app_config = if cfg!(windows) {
            std::env::var_os("APPDATA").map_or_else(|| home.clone(), PathBuf::from)
        } else if cfg!(target_os = "macos") {
            home.join("Library/Application Support")
        } else {
            xdg_config.clone()
        };
        Ok(Self {
            home,
            app_config,
            xdg_config,
        })
    }

    #[must_use]
    pub fn under(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            app_config: home.join("AppData/Roaming"),
            xdg_config: home.join(".config"),
        }
    }
}

/// How an agent spells a stdio MCP server in its config.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpShape {
    /// `{"type": "stdio", "command": ..., "args": [...]}`
    Stdio,
    /// `{"command": ..., "args": [...]}`
    Bare,
    /// opencode: `{"type": "local", "command": [...], "enabled": true}`
    Opencode,
    /// Codex: a `[mcp_servers.<name>]` table in TOML.
    CodexToml,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubagentShape {
    /// Markdown with `name` / `description` / `tools` front matter.
    Markdown,
    /// Codex: TOML with `developer_instructions`.
    CodexToml,
}

#[derive(Clone, Debug)]
pub struct Agent {
    pub id: &'static str,
    pub name: &'static str,
    /// On `PATH` means installed.
    pub binary: Option<&'static str>,
    /// Exists means installed.
    pub config_dir: Option<PathBuf>,
    /// File, dotted section path inside it, and the entry's shape.
    pub mcp: Option<(PathBuf, &'static str, McpShape)>,
    pub instructions: Option<PathBuf>,
    pub subagent: Option<(PathBuf, SubagentShape)>,
}

impl Agent {
    #[must_use]
    pub fn detected(&self) -> bool {
        self.binary.is_some_and(on_path) || self.config_dir.as_deref().is_some_and(is_lived_in)
    }
}

/// Whether a config directory belongs to an agent somebody uses. Installers of
/// other tools register themselves with every agent they know of and leave
/// `{"mcpServers": {}}` behind in directories they created; a directory that
/// holds nothing but such hollow stubs is not an installed agent.
fn is_lived_in(dir: &Path) -> bool {
    const MAX_DEPTH: usize = 3;
    fn any_real_file(dir: &Path, depth: usize) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        entries.flatten().any(|entry| {
            let path = entry.path();
            if path.is_dir() {
                return depth < MAX_DEPTH && any_real_file(&path, depth + 1);
            }
            let hollow_stub = path.extension().is_some_and(|extension| extension == "json")
                && std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                    .is_some_and(|value| super::config::is_hollow(&value));
            !hollow_stub
        })
    }
    any_real_file(dir, 0)
}

fn on_path(binary: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    let extensions: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat", ".ps1"]
    } else {
        &[""]
    };
    std::env::split_paths(&paths).any(|dir| {
        extensions
            .iter()
            .any(|extension| dir.join(format!("{binary}{extension}")).is_file())
    })
}

/// opencode reads `opencode.jsonc` or `opencode.json`; whichever exists wins.
fn opencode_config(dirs: &Dirs) -> PathBuf {
    let base = dirs.xdg_config.join("opencode");
    let json = base.join("opencode.json");
    if !base.join("opencode.jsonc").exists() && json.exists() {
        return json;
    }
    base.join("opencode.jsonc")
}

#[must_use]
pub fn agents(dirs: &Dirs) -> Vec<Agent> {
    use McpShape::{Bare, CodexToml, Opencode, Stdio};
    let home = &dirs.home;
    let markdown = |path: PathBuf| Some((path, SubagentShape::Markdown));
    vec![
        Agent {
            id: "claude",
            name: "Claude Code",
            binary: Some("claude"),
            config_dir: Some(home.join(".claude")),
            mcp: Some((home.join(".claude.json"), "mcpServers", Stdio)),
            instructions: Some(home.join(".claude/CLAUDE.md")),
            subagent: markdown(home.join(".claude/agents/omega.md")),
        },
        Agent {
            id: "gemini",
            name: "Gemini CLI",
            binary: Some("gemini"),
            config_dir: Some(home.join(".gemini")),
            mcp: Some((home.join(".gemini/settings.json"), "mcpServers", Stdio)),
            instructions: Some(home.join(".gemini/GEMINI.md")),
            subagent: markdown(home.join(".gemini/agents/omega.md")),
        },
        Agent {
            id: "kiro",
            name: "Kiro",
            binary: Some("kiro"),
            config_dir: Some(home.join(".kiro")),
            mcp: Some((home.join(".kiro/settings/mcp.json"), "mcpServers", Stdio)),
            instructions: Some(home.join(".kiro/steering/omega.md")),
            subagent: markdown(home.join(".kiro/agents/omega.md")),
        },
        Agent {
            id: "opencode",
            name: "Opencode",
            binary: Some("opencode"),
            config_dir: Some(dirs.xdg_config.join("opencode")),
            mcp: Some((opencode_config(dirs), "mcp", Opencode)),
            instructions: Some(dirs.xdg_config.join("opencode/AGENTS.md")),
            subagent: markdown(dirs.xdg_config.join("opencode/agents/omega.md")),
        },
        Agent {
            id: "codex",
            name: "Codex",
            binary: Some("codex"),
            config_dir: Some(home.join(".codex")),
            mcp: Some((home.join(".codex/config.toml"), "mcp_servers", CodexToml)),
            instructions: Some(home.join(".codex/AGENTS.md")),
            subagent: Some((home.join(".codex/agents/omega.toml"), SubagentShape::CodexToml)),
        },
        Agent {
            id: "vscode",
            name: "VS Code",
            binary: Some("code"),
            config_dir: None,
            mcp: Some((dirs.app_config.join("Code/User/mcp.json"), "servers", Stdio)),
            instructions: None,
            subagent: None,
        },
        Agent {
            id: "pi",
            name: "Pi",
            binary: Some("pi"),
            config_dir: Some(home.join(".pi")),
            mcp: Some((home.join(".pi/agent/mcp.json"), "mcpServers", Bare)),
            instructions: None,
            subagent: markdown(home.join(".pi/agents/omega.md")),
        },
        Agent {
            id: "cursor",
            name: "Cursor",
            binary: Some("cursor"),
            config_dir: Some(home.join(".cursor")),
            mcp: Some((home.join(".cursor/mcp.json"), "mcpServers", Stdio)),
            // Cursor's instructions are project-local .mdc files.
            instructions: None,
            subagent: markdown(home.join(".cursor/agents/omega.md")),
        },
        Agent {
            id: "copilot",
            name: "GitHub Copilot",
            binary: None,
            config_dir: Some(dirs.xdg_config.join("github-copilot")),
            mcp: Some((home.join(".copilot/mcp-config.json"), "mcpServers", Bare)),
            instructions: None,
            subagent: markdown(home.join(".copilot/agents/omega.agent.md")),
        },
        Agent {
            id: "zcode",
            name: "ZCode",
            binary: None,
            config_dir: Some(home.join(".zcode")),
            mcp: Some((home.join(".zcode/cli/config.json"), "mcp.servers", Stdio)),
            instructions: Some(home.join(".zcode/AGENTS.md")),
            subagent: markdown(home.join(".zcode/agents/omega.md")),
        },
        Agent {
            id: "windsurf",
            name: "Windsurf",
            binary: Some("windsurf"),
            config_dir: Some(home.join(".codeium/windsurf")),
            mcp: Some((home.join(".codeium/windsurf/mcp_config.json"), "mcpServers", Bare)),
            instructions: None,
            subagent: None,
        },
        Agent {
            id: "zed",
            name: "Zed",
            binary: Some("zed"),
            config_dir: Some(dirs.xdg_config.join("zed")),
            mcp: Some((dirs.xdg_config.join("zed/settings.json"), "context_servers", Bare)),
            instructions: None,
            subagent: None,
        },
        Agent {
            id: "reasonix",
            name: "Reasonix",
            binary: Some("reasonix"),
            config_dir: Some(dirs.xdg_config.join("reasonix")),
            mcp: Some((home.join(".reasonix/config.json"), "mcpServers", Bare)),
            instructions: Some(dirs.xdg_config.join("reasonix/REASONIX.md")),
            subagent: markdown(home.join(".reasonix/skills/omega.md")),
        },
        Agent {
            id: "commandcode",
            name: "Command Code",
            binary: None,
            config_dir: Some(home.join(".commandcode")),
            mcp: Some((home.join(".commandcode/mcp.json"), "mcpServers", Bare)),
            instructions: Some(home.join(".commandcode/AGENTS.md")),
            subagent: markdown(home.join(".commandcode/agents/omega.md")),
        },
        Agent {
            id: "antigravity",
            name: "Antigravity",
            binary: Some("agy"),
            config_dir: Some(home.join(".gemini/antigravity-cli")),
            mcp: Some((home.join(".gemini/config/mcp_config.json"), "mcpServers", Stdio)),
            instructions: Some(home.join(".gemini/GEMINI.md")),
            subagent: markdown(home.join(".gemini/config/skills/omega/SKILL.md")),
        },
    ]
}
