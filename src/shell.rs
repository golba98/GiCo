//! Manage only GiCo's marked Zsh startup block.
use std::{
    fs,
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

const LINE: &str = "(( $+commands[gico] )) && eval \"$(command gico init-zsh)\"";
const BEGIN: &str = "# >>> GiCo interactive shell >>>";
const END: &str = "# <<< GiCo interactive shell <<<";
const LEGACY: &str = "# Apply approved GiCo directory changes in this shell.";

pub fn configure(enable: bool) -> io::Result<PathBuf> {
    let dir = std::env::var_os("ZDOTDIR")
        .map(PathBuf::from)
        .unwrap_or(crate::config::home()?);
    let file = dir.join(".zshrc");
    let path = if file.exists() {
        file.canonicalize()?
    } else {
        file
    };
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("invalid Zsh config location"))?;
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(parent.join(".gico-zsh.lock"))?;
    lock.lock()?;
    let old = match fs::read_to_string(&path) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let block = format!("{BEGIN}\n{LINE}\n{END}\n");
    if old.contains(BEGIN) && !old.contains(&block) {
        return Err(io::Error::other(
            "edited GiCo startup block; preserve it and check the marker lines",
        ));
    }
    let legacy = format!("{LEGACY}\n{LINE}\n");
    let mut next = old.replace(&block, "").replace(&legacy, "");
    if enable && !next.lines().any(|l| l.trim() == LINE) {
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(&block);
    }
    if old != next {
        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        let mode = fs::metadata(&path).map_or(0o600, |m| m.permissions().mode());
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
        tmp.write_all(next.as_bytes())?;
        tmp.as_file().sync_all()?;
        tmp.persist(&path)?;
    }
    Ok(path)
}
