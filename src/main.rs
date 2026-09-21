use omega::index::Index;
use omega::search::{Content, Options, render, search};
use std::path::{Path, PathBuf};
use std::time::Instant;

const USAGE: &str = "\
omega search <query> [--root DIR] [-k N] [--content code|docs|config|all] [--path TEXT] [--lines N]
omega outline <file-or-directory> [--root DIR]
omega usages <symbol> [--root DIR] [-k N] [--path TEXT]
omega eval <probes.tsv>... [--root DIR] [--limit N] [--hits N]
omega mcp [--root DIR]
omega model install
omega install|uninstall [--agents claude,codex,...] [--integrations mcp,instructions,subagent] [--yes]

The static model is read from --model DIR, else OMEGA_MODEL, else where
`model install` put it, else the Hugging Face cache; without one, search is
lexical only.";

fn main() {
    if let Err(message) = run() {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let command = args.next().ok_or(USAGE)?;
    if matches!(command.as_str(), "--version" | "-V" | "version") {
        println!("omega {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let mut positional = Vec::new();
    let mut root = PathBuf::from(".");
    let mut model: Option<PathBuf> = std::env::var_os("OMEGA_MODEL").map(PathBuf::from);
    let mut options = Options::default();
    let mut eval_limit = 20usize;
    let mut limit_given = false;
    let mut eval_hits = None;
    let mut request = omega::install::Request::default();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--root" => root = PathBuf::from(value("--root")?),
            "--model" => model = Some(PathBuf::from(value("--model")?)),
            "-k" => {
                options.limit = number(&value("-k")?)?;
                limit_given = true;
            }
            "--limit" => eval_limit = number(&value("--limit")?)?,
            "--hits" => eval_hits = Some(number(&value("--hits")?)?),
            "--lines" => options.snippet_lines = number(&value("--lines")?)?,
            "--path" => options.path = Some(value("--path")?),
            "--content" => {
                options.content = Content::parse(&value("--content")?).ok_or("unknown --content")?;
            }
            "--agents" => {
                request.agents = Some(value("--agents")?.split(',').map(str::to_owned).collect());
            }
            "--integrations" => {
                let parsed: Option<Vec<_>> = value("--integrations")?
                    .split(',')
                    .map(omega::install::Integration::parse)
                    .collect();
                request.integrations = Some(parsed.ok_or("unknown --integrations; use mcp,instructions,subagent")?);
            }
            "--yes" | "-y" => request.yes = true,
            "--no-model" => model = Some(PathBuf::new()),
            _ => positional.push(arg),
        }
    }
    let model = match model {
        Some(path) if path.as_os_str().is_empty() => None,
        Some(path) => Some(path),
        None => omega::model::locate(),
    };

    // A root that is not there would index nothing and answer "no matches":
    // the agent would conclude the code does not exist.
    let reads_a_tree = matches!(command.as_str(), "search" | "outline" | "usages" | "eval" | "mcp");
    if reads_a_tree && !root.is_dir() {
        return Err(format!("--root {} is not a directory", root.display()));
    }

    match command.as_str() {
        "search" => {
            let query = positional.join(" ");
            let index = Index::open(&root, model.as_deref())?;
            let hits = search(&index, &query, &options);
            if index.model.is_none() {
                eprintln!("lexical only: run `omega model install` for the dense channel");
            }
            print!("{}", render(&index, &query, &hits, &options));
            Ok(())
        }
        "model" if positional.first().map(String::as_str) == Some("install") => {
            let dir = omega::model::install()?;
            println!("model installed at {}", dir.display());
            Ok(())
        }
        "install" => omega::install::run(omega::install::Mode::Install, request),
        "uninstall" => omega::install::run(omega::install::Mode::Uninstall, request),
        "outline" => {
            let index = Index::open(&root, None)?;
            print!("{}", omega::outline::outline(&index, &positional.join(" ")));
            Ok(())
        }
        "usages" => {
            let index = Index::open(&root, None)?;
            let mut wanted = omega::usages::Options {
                path: options.path.clone(),
                ..Default::default()
            };
            if limit_given {
                wanted.limit = options.limit;
            }
            print!("{}", omega::usages::usages(&index, &positional.join(" "), &wanted));
            Ok(())
        }
        "eval" => eval(&root, model.as_deref(), &positional, eval_limit, eval_hits),
        "mcp" => omega::mcp::serve(&root, model.as_deref()),
        _ => Err(USAGE.to_owned()),
    }
}

fn number(text: &str) -> Result<usize, String> {
    text.parse().map_err(|_| format!("not a number: {text}"))
}

/// Probes are `question<TAB>path[<TAB>anchor]`. A file rank is the place of the
/// expected path among the distinct files answered; with an anchor, a span rank
/// is the place of the first answered chunk whose own lines contain it -- what
/// says whether the agent was handed the lines, not just the right file.
/// `--hits 8` scores only the answers an agent actually receives.
fn eval(
    root: &Path,
    model: Option<&Path>,
    files: &[String],
    limit: usize,
    hits: Option<usize>,
) -> Result<(), String> {
    let started = Instant::now();
    let index = Index::open(root, model)?;
    println!(
        "indexed {} files, {} chunks in {:.2}s (model: {})",
        index.files.len(),
        index.chunks.len(),
        started.elapsed().as_secs_f32(),
        if index.model.is_some() { "yes" } else { "no" },
    );
    let options = Options {
        limit: hits.unwrap_or(limit * 8),
        ..Options::default()
    };
    let hits_given = hits.is_some();
    let show_misses = std::env::var_os("OMEGA_MISSES").is_some();
    for file in files {
        let text = std::fs::read_to_string(file).map_err(|error| format!("{file}: {error}"))?;
        let mut file_ranks: Vec<Option<usize>> = Vec::new();
        let mut span_ranks: Vec<Option<usize>> = Vec::new();
        let mut spent = std::time::Duration::ZERO;
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let mut columns = line.split('\t');
            let (Some(question), Some(expected)) = (columns.next(), columns.next()) else {
                continue;
            };
            let expected = expected.trim();
            let anchor = columns.next().map(str::trim).filter(|anchor| !anchor.is_empty());
            let asked = Instant::now();
            let mut hits = search(&index, question, &options);
            spent += asked.elapsed();
            if hits_given {
                hits = omega::search::present(&hits).0.into_iter().map(|(hit, _)| hit).collect();
            }

            let mut seen: Vec<&str> = Vec::new();
            for hit in &hits {
                let path = index.files[index.chunks[hit.chunk].file as usize].path.as_str();
                if !seen.contains(&path) {
                    seen.push(path);
                }
            }
            seen.truncate(limit);
            let file_rank = seen.iter().position(|path| path.contains(expected));
            file_ranks.push(file_rank);

            if let Some(anchor) = anchor {
                let source = std::fs::read_to_string(root.join(expected)).unwrap_or_default();
                let lines: Vec<&str> = source.lines().collect();
                let span_rank = hits.iter().take(limit).position(|hit| {
                    let chunk = &index.chunks[hit.chunk];
                    index.files[chunk.file as usize].path == expected
                        && lines
                            .iter()
                            .skip(hit.start_line as usize - 1)
                            .take((hit.end_line + 1 - hit.start_line) as usize)
                            .any(|line| line.contains(anchor))
                });
                span_ranks.push(span_rank);
            }
            if show_misses && file_rank.is_none_or(|rank| rank >= 5) {
                let place = file_rank.map_or("-".to_owned(), |rank| (rank + 1).to_string());
                println!("  {place:>4}  {question}  -> {expected}");
            }
        }
        let per_query = spent.as_secs_f32() * 1000.0 / file_ranks.len().max(1) as f32;
        println!("\n=== {file}  ({} probes, {per_query:.1} ms/query)", file_ranks.len());
        report("file", &file_ranks);
        if !span_ranks.is_empty() {
            report("span", &span_ranks);
        }
    }
    Ok(())
}

fn report(label: &str, ranks: &[Option<usize>]) {
    let total = ranks.len().max(1) as f32;
    let recall = |k: usize| ranks.iter().filter(|rank| rank.is_some_and(|rank| rank < k)).count() as f32 / total;
    let mrr: f32 = ranks.iter().map(|rank| rank.map_or(0.0, |rank| 1.0 / (rank as f32 + 1.0))).sum();
    println!(
        "  {label}  R@1 {:.3}  R@3 {:.3}  R@5 {:.3}  R@10 {:.3}  R@20 {:.3}  MRR {:.3}",
        recall(1), recall(3), recall(5), recall(10), recall(20), mrr / total
    );
}
