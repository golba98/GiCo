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
    let dir = crate::config::home()
        .map_err(|e| e.to_string())?
        .join(".cache/gico/lifecycle");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut key = DefaultHasher::new();
    (&cfg.endpoint, &cfg.model, cfg.context_size).hash(&mut key);
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join(format!("{:x}.lock", key.finish())))
        .map_err(|e| e.to_string())?;
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
    client.preload().map_err(|e| e.to_string())?;
    file.set_len(1).map_err(|e| e.to_string())?;
    file.set_modified(std::time::SystemTime::now())
        .map_err(|e| e.to_string())
}
