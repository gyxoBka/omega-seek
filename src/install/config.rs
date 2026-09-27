use serde_json::{Map, Value};
use std::path::Path;

pub const BLOCK_START: &str = "<!-- OMEGA_START -->";
pub const BLOCK_END: &str = "<!-- OMEGA_END -->";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    Created,
    Updated,
    Unchanged,
    Removed,
    NotFound,
    Skipped(String),
    Failed(String),
}

pub fn merge_json(path: &Path, section: &str, key: &str, value: &Value) -> Action {
    let existed = path.exists();
    let text = if existed {
        match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) => return Action::Failed(error.to_string()),
        }
    } else {
        String::new()
    };
    let mut root = if text.trim().is_empty() {
        Value::Object(Map::new())
    } else {
        match serde_json::from_str::<Value>(&text) {
            Ok(root @ Value::Object(_)) => root,
            Ok(_) => return Action::Failed("the top level is not an object".into()),
            Err(_) => return Action::Skipped("not plain JSON (comments?)".into()),
        }
    };
    let mut container = &mut root;
    for part in section.split('.') {
        let Some(object) = container.as_object_mut() else {
            return Action::Failed(format!("`{part}` sits under something that is not an object"));
        };
        container = object
            .entry(part.to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    let Some(object) = container.as_object_mut() else {
        return Action::Failed(format!("`{section}` is not an object"));
    };
    if object.get(key) == Some(value) {
        return Action::Unchanged;
    }
    object.insert(key.to_owned(), value.clone());
    match write(path, &pretty(&root), existed) {
        Ok(()) if existed => Action::Updated,
        Ok(()) => Action::Created,
        Err(error) => Action::Failed(error),
    }
}

pub fn remove_json(path: &Path, section: &str, key: &str) -> Action {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Action::NotFound;
    };
    let Ok(mut root) = serde_json::from_str::<Value>(&text) else {
        return if text.contains(&format!("\"{key}\"")) {
            Action::Skipped("not plain JSON (comments?)".into())
        } else {
            Action::NotFound
        };
    };
    let mut container = &mut root;
    for part in section.split('.') {
        match container.get_mut(part) {
            Some(next) => container = next,
            None => return Action::NotFound,
        }
    }
    let removed = container
        .as_object_mut()
        .and_then(|object| object.shift_remove(key));
    if removed.is_none() {
        return Action::NotFound;
    }
    let result = if is_hollow(&root) {
        std::fs::remove_file(path).map_err(|error| error.to_string())
    } else {
        write(path, &pretty(&root), false)
    };
    let _ = std::fs::remove_file(backup_of(path));
    match result {
        Ok(()) => Action::Removed,
        Err(error) => Action::Failed(error),
    }
}

pub(crate) fn is_hollow(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|object| object.values().all(is_hollow))
}

fn backup_of(path: &Path) -> std::path::PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{name}.omega.bak"))
}

fn pretty(root: &Value) -> String {
    let mut text = serde_json::to_string_pretty(root).unwrap_or_default();
    text.push('\n');
    text
}

pub fn merge_block(path: &Path, block: &str) -> Action {
    let existed = path.exists();
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let updated = match bounds(&existing) {
        Some((start, end)) => format!(
            "{}{}\n{}",
            &existing[..start],
            block.trim_matches('\n'),
            existing[end..].trim_start_matches('\n')
        ),
        None if existing.is_empty() => block.to_owned(),
        None => format!("{}\n\n{block}", existing.trim_end_matches('\n')),
    };
    if updated == existing {
        return Action::Unchanged;
    }
    match write(path, &updated, false) {
        Ok(()) if existed => Action::Updated,
        Ok(()) => Action::Created,
        Err(error) => Action::Failed(error),
    }
}

pub fn remove_block(path: &Path) -> Action {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Action::NotFound;
    };
    let Some((start, end)) = bounds(&existing) else {
        return Action::NotFound;
    };
    let before = existing[..start].trim_end_matches('\n');
    let after = existing[end..].trim_start_matches('\n');
    let remaining = match (before.is_empty(), after.is_empty()) {
        (true, true) => String::new(),
        (true, false) => after.to_owned(),
        (false, true) => format!("{before}\n"),
        (false, false) => format!("{before}\n\n{after}"),
    };
    let result = if remaining.is_empty() {
        std::fs::remove_file(path).map_err(|error| error.to_string())
    } else {
        write(path, &remaining, false)
    };
    match result {
        Ok(()) => Action::Removed,
        Err(error) => Action::Failed(error),
    }
}

fn bounds(text: &str) -> Option<(usize, usize)> {
    let start = text.find(BLOCK_START)?;
    let end = start + text[start..].find(BLOCK_END)? + BLOCK_END.len();
    Some((start, end))
}

pub fn merge_toml_table(path: &Path, header: &str, table: &str) -> Action {
    let existed = path.exists();
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.contains(table.trim_end()) {
        return Action::Unchanged;
    }
    let rest = without_toml_table(&existing, header);
    let rest = rest.trim_end_matches('\n');
    let updated = if rest.is_empty() {
        table.to_owned()
    } else {
        format!("{rest}\n\n{table}")
    };
    match write(path, &updated, existed) {
        Ok(()) if existed => Action::Updated,
        Ok(()) => Action::Created,
        Err(error) => Action::Failed(error),
    }
}

pub fn remove_toml_table(path: &Path, header: &str) -> Action {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Action::NotFound;
    };
    if !existing.contains(&format!("[{header}]")) {
        return Action::NotFound;
    }
    let remaining = without_toml_table(&existing, header);
    let remaining = remaining.trim_matches('\n');
    let result = if remaining.is_empty() {
        std::fs::remove_file(path).map_err(|error| error.to_string())
    } else {
        write(path, &format!("{remaining}\n"), false)
    };
    let _ = std::fs::remove_file(backup_of(path));
    match result {
        Ok(()) => Action::Removed,
        Err(error) => Action::Failed(error),
    }
}

fn without_toml_table(text: &str, header: &str) -> String {
    let mut kept = String::new();
    let mut skipping = false;
    for line in text.split_inclusive('\n') {
        let head = line.split('#').next().unwrap_or_default().trim();
        if let Some(name) = head.strip_prefix('[').and_then(|head| head.strip_suffix(']')) {
            let name = name.trim_matches(['[', ']']);
            skipping = name == header || name.starts_with(&format!("{header}."));
        }
        if !skipping {
            kept.push_str(line);
        }
    }
    kept
}

pub fn write_file(path: &Path, content: &str) -> Action {
    let existed = path.exists();
    if existed && std::fs::read_to_string(path).is_ok_and(|existing| existing == content) {
        return Action::Unchanged;
    }
    match write(path, content, false) {
        Ok(()) if existed => Action::Updated,
        Ok(()) => Action::Created,
        Err(error) => Action::Failed(error),
    }
}

pub fn remove_file(path: &Path) -> Action {
    if !path.exists() {
        return Action::NotFound;
    }
    match std::fs::remove_file(path) {
        Ok(()) => Action::Removed,
        Err(error) => Action::Failed(error.to_string()),
    }
}

fn write(path: &Path, content: &str, backup: bool) -> Result<(), String> {
    let describe = |error: std::io::Error| format!("{}: {error}", path.display());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(describe)?;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if backup && path.exists() {
        std::fs::copy(path, backup_of(path)).map_err(describe)?;
    }
    let aside = path.with_file_name(format!("{name}.omega.tmp"));
    std::fs::write(&aside, content).map_err(describe)?;
    std::fs::rename(&aside, path).map_err(describe)
}
