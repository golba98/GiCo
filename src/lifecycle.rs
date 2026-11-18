use crate::{config::Config, llm::Backend};
use std::{
    fs::{File, OpenOptions},
    hash::{DefaultHasher, Hash, Hasher},
    os::unix::fs::OpenOptionsExt,
};

/// Per-runner advisory lock. The OS releases it if the loading client exits.
pub fn warm(cfg: &Config) -> Result<(), String> {
    let client = cfg.client()?;
    if !cfg.preload || client.backend != Backend::Ollama || cfg.idle_seconds == 0 {
        return Ok(());
    }
    let file = lockfile(&cfg.endpoint, &cfg.model, cfg.context_size, "warm")?;
    match File::try_lock(&file) {
        Ok(()) => (),
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(e) => return Err(e.to_string()),
    }
    // Coalesce adjacent successful opens. Always query residency after restart.
    if file
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|d| d.as_secs() < 2)
        && file.metadata().is_ok_and(|m| m.len() > 0)
        && client.resident().unwrap_or(false)
    {
        return Ok(());
    }
    let _loading = loading_marker(&cfg.endpoint, &cfg.model, cfg.context_size);
    client.preload().map_err(|e| e.to_string())?;
    file.set_len(1).map_err(|e| e.to_string())?;
    file.set_modified(std::time::SystemTime::now())
        .map_err(|e| e.to_string())
}

fn lockfile(endpoint: &str, model: &str, context: u32, role: &str) -> Result<File, String> {
    let dir = crate::config::home()
        .map_err(|e| e.to_string())?
        .join(".cache/gico/lifecycle");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut key = DefaultHasher::new();
    (endpoint, model, context).hash(&mut key);
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(format!("{:x}.{role}.lock", key.finish())))
        .map_err(|e| e.to_string())
}

pub fn loading_marker(endpoint: &str, model: &str, context: u32) -> Option<File> {
    let file = lockfile(endpoint, model, context, "load").ok()?;
    file.try_lock_shared().ok()?;
    Some(file)
}

pub fn loading(cfg: &Config) -> bool {
    lockfile(&cfg.endpoint, &cfg.model, cfg.context_size, "load")
        .is_ok_and(|f| matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock)))
}
