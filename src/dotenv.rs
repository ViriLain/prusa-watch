//! Minimal .env loader so native runs pick up the same file docker compose uses.
//!
//! Rules (a compatible subset of docker compose / python-dotenv):
//!   - blank lines and lines starting with # are ignored; an optional `export ` prefix is allowed
//!   - KEY=VALUE; VALUE may be wrapped in single or double quotes
//!   - unquoted values lose a trailing ` # comment`
//!   - variables already set in the environment win (the shell overrides the file)
//!   - empty values are skipped, so `${VAR:-default}` in config.yaml still falls back

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

static LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*?)\s*$").unwrap());
static COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+#").unwrap());

pub fn parse_dotenv(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for raw in text.lines() {
        if raw.trim().is_empty() || raw.trim_start().starts_with('#') {
            continue;
        }
        let Some(c) = LINE.captures(raw) else { continue };
        let key = c[1].to_string();
        let mut val = c[2].to_string();
        let b = val.as_bytes();
        if b.len() >= 2 && b[0] == b[b.len() - 1] && (b[0] == b'\'' || b[0] == b'"') {
            val = val[1..val.len() - 1].to_string();
        } else {
            val = COMMENT.splitn(&val, 2).next().unwrap_or("").trim_end().to_string();
        }
        out.insert(key, val);
    }
    out
}

/// Environment access, abstracted so tests don't touch the process environment.
pub trait EnvStore {
    fn contains(&self, key: &str) -> bool;
    fn set(&mut self, key: &str, value: &str);
}

pub struct ProcessEnv;

impl EnvStore for ProcessEnv {
    fn contains(&self, key: &str) -> bool {
        std::env::var_os(key).is_some()
    }
    fn set(&mut self, key: &str, value: &str) {
        // SAFETY: called at startup before any other threads exist.
        unsafe { std::env::set_var(key, value) }
    }
}

impl EnvStore for BTreeMap<String, String> {
    fn contains(&self, key: &str) -> bool {
        self.contains_key(key)
    }
    fn set(&mut self, key: &str, value: &str) {
        self.insert(key.to_string(), value.to_string());
    }
}

/// Load each existing file in `paths` (first file wins per key). Returns the files loaded.
pub fn load_dotenv(paths: &[PathBuf], env: &mut dyn EnvStore) -> Vec<PathBuf> {
    let mut loaded = Vec::new();
    let mut seen = HashSet::new();
    for p in paths {
        let Ok(path) = std::fs::canonicalize(p) else { continue };
        if !seen.insert(path.clone()) || !path.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        for (k, v) in parse_dotenv(&text) {
            if !v.is_empty() && !env.contains(&k) {
                env.set(&k, &v);
            }
        }
        loaded.push(path);
    }
    loaded
}

pub fn default_candidates(config_path: &Path) -> Vec<PathBuf> {
    let dir = std::path::absolute(config_path).ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_default();
    vec![dir.join(".env"), std::env::current_dir().unwrap_or_default().join(".env")]
}
