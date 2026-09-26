//! `omega install` / `uninstall`: wire the search into coding agents.
//!
//! Three integrations per agent, each independent: the MCP server entry, a
//! marked block of standing instructions, and a sub-agent that uses the CLI.
//! Everything written is either an entry under our own key, a block between
//! our own markers, or a file of our own, so uninstall takes out exactly what
//! install put in.

pub mod agents;
pub mod config;

use agents::{Agent, Dirs, McpShape, SubagentShape};
use config::Action;
use serde_json::{Value, json};
use std::path::Path;

pub const SERVER_NAME: &str = "omega";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Install,
    Uninstall,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Integration {
    Mcp,
    Instructions,
    Subagent,
}

impl Integration {
    pub const ALL: [Self; 3] = [Self::Mcp, Self::Instructions, Self::Subagent];

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "mcp" => Some(Self::Mcp),
            "instructions" => Some(Self::Instructions),
            "subagent" => Some(Self::Subagent),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Mcp => "MCP server",
            Self::Instructions => "Instructions",
            Self::Subagent => "Sub-agent",
        }
    }

    fn explanation(self) -> &'static str {
        match self {
            Self::Mcp => "lets the agent call omega directly as a tool",
            Self::Instructions => "adds usage guidance to AGENTS.md / CLAUDE.md",
            Self::Subagent => "installs a dedicated omega sub-agent",
        }
    }

    fn target(self, agent: &Agent) -> Option<&Path> {
        match self {
            Self::Mcp => agent.mcp.as_ref().map(|(path, ..)| path.as_path()),
            Self::Instructions => agent.instructions.as_deref(),
            Self::Subagent => agent.subagent.as_ref().map(|(path, _)| path.as_path()),
        }
    }
}

/// What was asked for on the command line; anything absent is asked for.
#[derive(Debug, Default)]
pub struct Request {
    pub agents: Option<Vec<String>>,
    pub integrations: Option<Vec<Integration>>,
    pub yes: bool,
    /// Show the plan and write nothing.
    pub dry_run: bool,
    /// Write again only what is already installed, wherever it is: what an
    /// update does, so that a text that changed with the release reaches the
    /// agents and no agent gains an integration it was never given.
    pub refresh: bool,
}

/// Whether `integration` is already installed into `agent`: our entry under
/// its key, our block between our markers, our file.
#[must_use]
pub fn installed(agent: &Agent, integration: Integration) -> bool {
    let holds = |path: &Path, mark: &str| std::fs::read_to_string(path).is_ok_and(|text| text.contains(mark));
    match integration {
        Integration::Mcp => agent.mcp.as_ref().is_some_and(|(path, section, shape)| match shape {
            McpShape::CodexToml => holds(path, &format!("[{section}.{SERVER_NAME}]")),
            _ => std::fs::read_to_string(path)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .is_some_and(|root| root.get(section).and_then(|s| s.get(SERVER_NAME)).is_some()),
        }),
        Integration::Instructions => agent.instructions.as_deref().is_some_and(|path| holds(path, config::BLOCK_START)),
        Integration::Subagent => agent.subagent.as_ref().is_some_and(|(path, _)| path.is_file()),
    }
}

pub fn run(mode: Mode, request: Request) -> Result<(), String> {
    let dirs = Dirs::discover()?;
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let known = agents::agents(&dirs);
    let title = if mode == Mode::Install { "Installer" } else { "Uninstaller" };
    println!("\n  omega {title}\n");
    let built_in_place = exe.components().any(|part| part.as_os_str() == "target");
    if mode == Mode::Install && built_in_place {
        println!(
            "  Note: agents will be pointed at {}\n  which a `cargo clean` removes. `cargo install --path .` puts a copy\n  in ~/.cargo/bin; run `omega install` from there for a path that stays.\n",
            exe.display()
        );
    }

    let chosen: Vec<&Agent> = match &request.agents {
        Some(ids) => {
            if let Some(unknown) = ids.iter().find(|id| known.iter().all(|agent| agent.id != id.as_str())) {
                let all: Vec<&str> = known.iter().map(|agent| agent.id).collect();
                return Err(format!("unknown agent `{unknown}`; known: {}", all.join(", ")));
            }
            known.iter().filter(|agent| ids.iter().any(|id| id == agent.id)).collect()
        }
        // A refresh goes only where something of ours already is.
        None if request.refresh => known
            .iter()
            .filter(|agent| Integration::ALL.iter().any(|&integration| installed(agent, integration)))
            .collect(),
        // `--yes` asks nothing: what would have been ticked is what is chosen.
        None if request.yes || request.dry_run => known.iter().filter(|agent| agent.detected()).collect(),
        None => {
            // Detected agents first, and ticked.
            let mut order: Vec<&Agent> = known.iter().collect();
            order.sort_by_key(|agent| !agent.detected());
            let labels: Vec<String> = order
                .iter()
                .map(|agent| {
                    let detected = if agent.detected() { "  (detected)" } else { "" };
                    format!("{}{detected}", agent.name)
                })
                .collect();
            let ticked: Vec<usize> = (0..order.len()).filter(|&at| order[at].detected()).collect();
            let picked = select("Select agents to configure:", labels, &ticked)?;
            picked.into_iter().map(|at| order[at]).collect()
        }
    };
    if chosen.is_empty() {
        println!("{}", if request.refresh { "  Nothing of omega's is installed into any agent." } else { "  Nothing selected." });
        return Ok(());
    }

    let integrations: Vec<Integration> = match &request.integrations {
        Some(integrations) => integrations.clone(),
        None if request.yes || request.dry_run => Integration::ALL.to_vec(),
        None => {
            let labels = Integration::ALL
                .iter()
                .map(|integration| format!("{:<13} -  {}", integration.label(), integration.explanation()))
                .collect();
            let picked = select("Select integrations to enable:", labels, &[0, 1, 2])?;
            picked.into_iter().map(|at| Integration::ALL[at]).collect()
        }
    };

    println!("\n  Plan:\n");
    for agent in &chosen {
        println!("  {}", agent.name);
        for integration in &integrations {
            match integration.target(agent) {
                Some(path) => println!("    {:<13} {}", integration.label(), display(&dirs, path)),
                None => println!("    {:<13} (not supported by this agent)", integration.label()),
            }
        }
        println!();
    }
    if request.dry_run {
        println!("  Dry run: nothing was written.
");
        return Ok(());
    }
    if !request.yes {
        let proceed = inquire::Confirm::new("Proceed?")
            .with_default(true)
            .prompt()
            .map_err(|error| error.to_string())?;
        if !proceed {
            println!("  Nothing changed.");
            return Ok(());
        }
    }

    for agent in &chosen {
        println!("  {}", agent.name);
        for &integration in &integrations {
            let Some(path) = integration.target(agent) else { continue };
            if request.refresh && !installed(agent, integration) {
                println!("    {:<13} {:<28} {}", integration.label(), "not installed, left so", display(&dirs, path));
                continue;
            }
            let action = apply(mode, agent, integration, &exe);
            println!("    {:<13} {:<28} {}", integration.label(), describe(&action), display(&dirs, path));
            if let (Action::Skipped(_), Integration::Mcp, Mode::Install) = (&action, integration, mode) {
                if let Some((_, section, shape)) = &agent.mcp {
                    println!("      add by hand under `{section}`:  \"{SERVER_NAME}\": {}", mcp_entry(*shape, &exe));
                }
            }
        }
        println!();
    }
    if mode == Mode::Install && crate::model::locate().is_none() {
        println!("  The model is not installed yet: run `omega model install` for semantic ranking.\n");
    }
    Ok(())
}

/// One integration of one agent, installed or taken out.
#[must_use]
pub fn apply(mode: Mode, agent: &Agent, integration: Integration, exe: &Path) -> Action {
    match (integration, mode) {
        (Integration::Mcp, _) => {
            let Some((path, section, shape)) = &agent.mcp else {
                return Action::NotFound;
            };
            match (shape, mode) {
                (McpShape::CodexToml, Mode::Install) => {
                    config::merge_toml_table(path, &format!("{section}.{SERVER_NAME}"), &codex_table(section, exe))
                }
                (McpShape::CodexToml, Mode::Uninstall) => {
                    config::remove_toml_table(path, &format!("{section}.{SERVER_NAME}"))
                }
                (_, Mode::Install) => config::merge_json(path, section, SERVER_NAME, &mcp_entry(*shape, exe)),
                (_, Mode::Uninstall) => config::remove_json(path, section, SERVER_NAME),
            }
        }
        (Integration::Instructions, Mode::Install) => agent
            .instructions
            .as_deref()
            .map_or(Action::NotFound, |path| config::merge_block(path, &instructions())),
        (Integration::Instructions, Mode::Uninstall) => agent
            .instructions
            .as_deref()
            .map_or(Action::NotFound, config::remove_block),
        (Integration::Subagent, Mode::Install) => agent
            .subagent
            .as_ref()
            .map_or(Action::NotFound, |(path, shape)| config::write_file(path, &subagent(*shape))),
        (Integration::Subagent, Mode::Uninstall) => agent
            .subagent
            .as_ref()
            .map_or(Action::NotFound, |(path, _)| config::remove_file(path)),
    }
}

fn mcp_entry(shape: McpShape, exe: &Path) -> Value {
    let exe = exe.to_string_lossy();
    match shape {
        McpShape::Stdio => json!({"type": "stdio", "command": exe, "args": ["mcp"]}),
        McpShape::Bare | McpShape::CodexToml => json!({"command": exe, "args": ["mcp"]}),
        McpShape::Opencode => json!({"type": "local", "command": [exe, "mcp"], "enabled": true}),
    }
}

fn codex_table(section: &str, exe: &Path) -> String {
    // A literal string: a Windows path needs no escaping in single quotes.
    format!("[{section}.{SERVER_NAME}]\ncommand = '{}'\nargs = [\"mcp\"]\n", exe.display())
}

/// What the agent is told, kept as Markdown beside this file so the wording can
/// be worked on without touching code. It names the command, not where the
/// binary lives: the agent calls the MCP tools, and a shell finds `omega`
/// on PATH. Only the MCP entry carries the absolute path, because an agent is
/// not always started with the PATH a terminal has.
const INSTRUCTIONS: &str = include_str!("instructions.md");
const SUBAGENT: &str = include_str!("subagent.md");

fn instructions() -> String {
    format!("{}\n{}\n{}\n", config::BLOCK_START, INSTRUCTIONS.trim(), config::BLOCK_END)
}

const SUBAGENT_DESCRIPTION: &str = "Code search agent for this repository. Use for locating \
implementations, finding where something is handled, listing the usages of an identifier or a \
message, and outlining files. Prefer over Grep/Glob for any exploratory question.";

fn subagent(shape: SubagentShape) -> String {
    let body = format!("{}\n", SUBAGENT.trim());
    match shape {
        SubagentShape::Markdown => format!(
            "---\nname: omega\ndescription: {SUBAGENT_DESCRIPTION}\ntools: Bash, Read\n---\n\n{body}"
        ),
        SubagentShape::OpenCodeMarkdown => format!(
            "---\nname: omega\ndescription: {SUBAGENT_DESCRIPTION}\ntools:\n  \"*\": false\n  bash: true\n  read: true\n---\n\n{body}"
        ),
        SubagentShape::CodexToml => format!(
            "name = \"omega\"\ndescription = \"{SUBAGENT_DESCRIPTION}\"\ndeveloper_instructions = '''\n{body}'''\n"
        ),
    }
}

fn describe(action: &Action) -> String {
    match action {
        Action::Created => "created".into(),
        Action::Updated => "updated".into(),
        Action::Unchanged => "unchanged".into(),
        Action::Removed => "removed".into(),
        Action::NotFound => "not installed".into(),
        Action::Skipped(why) => format!("skipped: {why}"),
        Action::Failed(why) => format!("FAILED: {why}"),
    }
}

fn display(dirs: &Dirs, path: &Path) -> String {
    match path.strip_prefix(&dirs.home) {
        Ok(relative) => format!("~/{}", relative.display()).replace('\\', "/"),
        Err(_) => path.display().to_string(),
    }
}

fn select(prompt: &str, labels: Vec<String>, ticked: &[usize]) -> Result<Vec<usize>, String> {
    let picked = inquire::MultiSelect::new(prompt, labels)
        .with_default(ticked)
        .with_page_size(15)
        .with_help_message("up/down move, space select, right all, left none, enter confirm")
        .raw_prompt()
        .map_err(|error| error.to_string())?;
    Ok(picked.into_iter().map(|option| option.index).collect())
}
