//! Provider API keys — resolved from the environment overlaid on a `0600` JSON
//! file. `OCCIPITAL_<PROVIDER>_KEY` always wins (ops/secret-manager override);
//! the file is the persisted store the CLI/API manage. Keys are never logged or
//! returned in full — only redacted previews.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct Keys {
    file:   PathBuf,
    stored: BTreeMap<String, String>,
}

impl Keys {
    /// Load the persisted key file (absent/unreadable → empty, never errors).
    pub fn load(file: &Path) -> Self {
        let stored = std::fs::read_to_string(file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { file: file.to_path_buf(), stored }
    }

    /// Resolve a provider's key: env `OCCIPITAL_<PROVIDER>_KEY` first, else file.
    pub fn get(&self, provider: &str) -> Option<String> {
        let env = format!("OCCIPITAL_{}_KEY", provider.to_ascii_uppercase());
        if let Ok(v) = std::env::var(&env) {
            if !v.is_empty() {
                return Some(v);
            }
        }
        self.stored.get(&provider.to_ascii_lowercase()).cloned()
    }

    pub fn set(&mut self, provider: &str, key: &str) {
        self.stored.insert(provider.to_ascii_lowercase(), key.to_string());
    }

    pub fn remove(&mut self, provider: &str) -> bool {
        self.stored.remove(&provider.to_ascii_lowercase()).is_some()
    }

    /// Stored providers with a **redacted** preview (never the full key).
    pub fn list(&self) -> Vec<(String, String)> {
        self.stored.iter().map(|(p, k)| (p.clone(), redact(k))).collect()
    }

    /// Persist the store as `0600` JSON.
    pub fn save(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.file.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&self.file, serde_json::to_string_pretty(&self.stored)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.file, std::fs::Permissions::from_mode(0o600)).ok();
        }
        Ok(())
    }
}

fn redact(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 4 {
        return "****".into();
    }
    let n = chars.len();
    format!("{}{}…{}{}", chars[0], chars[1], chars[n - 2], chars[n - 1])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn set_save_load_roundtrips_and_redacts() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("keys.json");
        {
            let mut k = Keys::load(&file);
            k.set("Brave", "secrettoken123");
            k.save().unwrap();
        }
        let k = Keys::load(&file);
        assert_eq!(k.get("brave").as_deref(), Some("secrettoken123"), "case-insensitive");
        let listed = k.list();
        assert_eq!(listed[0].0, "brave");
        assert!(!listed[0].1.contains("secret"), "list redacts: {}", listed[0].1);
        assert!(listed[0].1.contains('…'));
    }

    #[test]
    fn remove_works() {
        let dir = TempDir::new().unwrap();
        let mut k = Keys::load(&dir.path().join("k.json"));
        k.set("tavily", "x");
        assert!(k.remove("Tavily"));
        assert!(!k.remove("tavily"), "second remove is a no-op");
        assert!(k.get("tavily").is_none());
    }

    #[test]
    fn env_overrides_file() {
        let dir = TempDir::new().unwrap();
        let mut k = Keys::load(&dir.path().join("k.json"));
        k.set("bing", "fromfile");
        std::env::set_var("OCCIPITAL_BING_KEY", "fromenv");
        assert_eq!(k.get("bing").as_deref(), Some("fromenv"), "env wins");
        std::env::remove_var("OCCIPITAL_BING_KEY");
        assert_eq!(k.get("bing").as_deref(), Some("fromfile"), "falls back to file");
    }
}
