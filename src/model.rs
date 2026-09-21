//! Where the static model lives, and how it gets there.
//!
//! The model is three files at a pinned revision, each checked against the
//! digest recorded here, so what is loaded is what the numbers in the README
//! were measured with.

use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

const REPOSITORY: &str = "minishlab/potion-code-16M-v2";
const REVISION: &str = "e9d2a44ca6a05ac6685f3b23709ea57eb7352d5b";
const FILES: &[(&str, &str)] = &[
    ("config.json", "148e5691a6fcc553437156859701fba017a1ba5d340b170f17e0f3668fb861a7"),
    ("tokenizer.json", "107bbdcbad4bff1d299b7a4c3a2fb17c52890688b7dd0e4c9deab79d3c4f3d45"),
    ("model.safetensors", "75cf7a6c2171b230ad19b1e7d8e0b1aee86da5a02af8e7cacedd9921d227623c"),
];
/// Nothing the model ships is larger than this; a response that is, is not it.
const MAX_BYTES: u64 = 64 << 20;

/// The model to load: the installed one, else one Hugging Face already cached.
#[must_use]
pub fn locate() -> Option<PathBuf> {
    installed_dir()
        .filter(|dir| is_complete(dir))
        .or_else(hugging_face_cache)
}

/// Download the pinned model into the data directory, verifying every file.
pub fn install() -> Result<PathBuf, String> {
    let dir = installed_dir().ok_or("no home directory to install the model under")?;
    if is_complete(&dir) {
        return Ok(dir);
    }
    std::fs::create_dir_all(&dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    for (name, expected) in FILES {
        let url = format!("https://huggingface.co/{REPOSITORY}/resolve/{REVISION}/{name}");
        eprintln!("downloading {name}");
        let mut bytes = Vec::new();
        ureq::get(&url)
            .call()
            .map_err(|error| format!("{url}: {error}"))?
            .into_body()
            .into_reader()
            .take(MAX_BYTES)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("{url}: {error}"))?;
        let digest = hex(&Sha256::digest(&bytes));
        if digest != *expected {
            return Err(format!("{name}: digest {digest} is not the pinned {expected}"));
        }
        // Written aside and renamed, so an interrupted install is not mistaken
        // for a complete one.
        let partial = dir.join(format!("{name}.partial"));
        std::fs::write(&partial, &bytes)
            .and_then(|()| std::fs::rename(&partial, dir.join(name)))
            .map_err(|error| format!("{}: {error}", dir.join(name).display()))?;
    }
    Ok(dir)
}

fn is_complete(dir: &Path) -> bool {
    FILES.iter().all(|(name, _)| dir.join(name).is_file())
}

fn installed_dir() -> Option<PathBuf> {
    let data = if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
    } else if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(data)
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".local/share")
    };
    Some(data.join("omega").join("models").join(format!("potion-code-16M-v2-{}", &REVISION[..8])))
}

fn hugging_face_cache() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    let snapshot = Path::new(&home)
        .join(".cache/huggingface/hub/models--minishlab--potion-code-16M-v2/snapshots")
        .join(REVISION);
    is_complete(&snapshot).then_some(snapshot)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
