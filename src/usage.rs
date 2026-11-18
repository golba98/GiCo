//! Aggregate Responses usage. Contains only counters; never stores prompts or credentials.
use crate::{config, responses::Usage};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
};

#[derive(Default, Deserialize, Serialize)]
pub struct Totals {
    pub responses_requests: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub elapsed_ms: u128,
    pub estimated_usd: f64,
    pub unpriced_responses: u64,
}

fn path() -> io::Result<PathBuf> {
    Ok(std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|v| v.is_absolute())
        .unwrap_or(config::home()?.join(".local/share"))
        .join("gico/openai-usage.json"))
}
pub fn read() -> io::Result<Totals> {
    match fs::read(path()?) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Totals::default()),
        Err(e) => Err(e),
    }
}
pub fn record(current: &Usage) -> io::Result<()> {
    if current.attempts == 0 {
        return Ok(());
    }
    let p = path()?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(p.parent().expect("usage directory"))?;
    let lock = fs::OpenOptions::new()
        .mode(0o600)
        .create(true)
        .truncate(false)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(p.with_extension("lock"))?;
    lock.lock()?;
    let mut total = read()?;
    total.responses_requests += u64::from(current.attempts);
    total.input_tokens += current.input;
    total.cached_input_tokens += current.cached;
    total.output_tokens += current.output;
    total.reasoning_tokens += current.reasoning;
    total.elapsed_ms += current.latency_ms;
    if let Some(cost) = current.estimated_usd {
        total.estimated_usd += cost;
    } else {
        total.unpriced_responses += 1;
    }
    config::private_write(&p, &serde_json::to_vec(&total)?)
}
