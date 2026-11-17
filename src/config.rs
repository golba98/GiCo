//! Shared provider settings and atomic, private persistence.
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub model: String,
    pub backend: String,
    pub endpoint: String,
    pub budget_tokens: usize,
    pub context_size: u32,
    pub max_tokens: u32,
    pub temperature: f32,
    pub preload: bool,
    pub idle_seconds: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: "qwen2.5-coder:0.5b".into(),
            backend: "ollama".into(),
            endpoint: "http://127.0.0.1:11434".into(),
            budget_tokens: 700,
            context_size: 4096,
            max_tokens: 256,
            temperature: 0.1,
            preload: true,
            idle_seconds: 300,
        }
    }
}

pub fn home() -> io::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or_else(|| io::Error::other("HOME must be an absolute path"))
}

pub fn path() -> io::Result<PathBuf> {
    Ok(home()?.join(".config/gico/config.toml"))
}

pub fn private_write(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent directory"))?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    Ok(())
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let mut cfg: Self = match fs::read_to_string(path) {
            Ok(data) => {
                toml::from_str(&data).map_err(|e| format!("invalid {}: {e}", path.display()))?
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e.to_string()),
        };
        for (key, value) in [
            ("GICO_MODEL", &mut cfg.model),
            ("GICO_BACKEND", &mut cfg.backend),
            ("GICO_ENDPOINT", &mut cfg.endpoint),
        ] {
            if let Ok(s) = std::env::var(key) {
                *value = s;
            }
        }
        if let Ok(s) = std::env::var("GICO_BUDGET") {
            cfg.budget_tokens = s.parse().map_err(|_| "invalid GICO_BUDGET")?;
        }
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let mut table: toml::Table = match fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => toml::Table::new(),
            Err(e) => return Err(e),
        };
        let effective = toml::Table::try_from(self).map_err(io::Error::other)?;
        if effective
            .iter()
            .all(|(key, value)| table.get(key) == Some(value))
        {
            return Ok(());
        }
        table.extend(effective);
        private_write(
            path,
            toml::to_string_pretty(&table)
                .map_err(io::Error::other)?
                .as_bytes(),
        )
    }

    pub fn client(&self) -> Result<crate::llm::LlmClient, String> {
        use crate::llm::{Backend, LlmClient};
        let backend = Backend::parse(&self.backend)
            .ok_or_else(|| format!("unknown backend {}", self.backend))?;
        let url =
            reqwest::Url::parse(&self.endpoint).map_err(|_| "invalid provider endpoint URL")?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
        {
            return Err(
                "use an HTTP endpoint without embedded credentials or query parameters".into(),
            );
        }
        if self.model.chars().any(char::is_control)
            || self.model.trim().is_empty()
            || !(512..=65536).contains(&self.context_size)
            || self.max_tokens == 0
            || self.max_tokens >= self.context_size
            || !(0.0..=2.0).contains(&self.temperature)
            || self.idle_seconds > 86400
            || !(64..=16384).contains(&self.budget_tokens)
        {
            return Err(
                "invalid model, token budget, context, temperature or idle duration".into(),
            );
        }
        let mut client = LlmClient::new(backend, &self.endpoint, &self.model);
        client.temperature = self.temperature;
        client.max_tokens = self.max_tokens;
        client.context_size = self.context_size;
        client.idle_seconds = self.idle_seconds;
        Ok(client)
    }
}
