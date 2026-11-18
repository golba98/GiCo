//! The native API key stays in the caller's environment or Linux Secret Service.
use std::io::{IsTerminal, Write};
use std::process::{Command, Stdio};

const ATTRIBUTES: &[&str] = &["application", "gico", "provider", "openai"];

fn keyring(operation: &str) -> Command {
    let mut cmd = Command::new("/usr/bin/timeout");
    cmd.args([
        if operation == "lookup" { "5s" } else { "60s" },
        "/usr/bin/secret-tool",
        operation,
    ]);
    if operation == "store" {
        cmd.arg("--label=GiCo OpenAI API");
    }
    cmd.args(ATTRIBUTES)
        .env_remove("OPENAI_API_KEY")
        .stderr(Stdio::null());
    cmd
}

fn valid(key: &str) -> bool {
    !key.is_empty() && key.len() <= 1024 && key.bytes().all(|b| b.is_ascii_graphic())
}

pub fn credential() -> Result<String, String> {
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        return valid(&key)
            .then_some(key)
            .ok_or_else(|| "OPENAI_API_KEY is empty or malformed".into());
    }
    let result = keyring("lookup")
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "Secret Service unavailable. Set OPENAI_API_KEY or install secret-tool.")?;
    if result.status.success() {
        if let Ok(raw) = String::from_utf8(result.stdout) {
            let key = raw.trim_end_matches(['\r', '\n']);
            if valid(key) {
                return Ok(key.to_owned());
            }
        }
    }
    Err("OpenAI authentication unavailable. Run gico auth login or set OPENAI_API_KEY.".into())
}

pub fn login() -> Result<(), String> {
    let key = match std::env::var("OPENAI_API_KEY") {
        Ok(key) => key,
        Err(_) if std::io::stdin().is_terminal() => rpassword::prompt_password("OpenAI API key: ")
            .map_err(|_| "could not read the hidden terminal prompt")?,
        Err(_) => {
            return Err(
                "Open an interactive terminal for gico auth login, or set OPENAI_API_KEY.".into(),
            )
        }
    };
    if !valid(&key) {
        return Err("Empty or malformed API key.".into());
    }
    let mut child = keyring("store")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .map_err(|_| "could not start secret-tool")?;
    let written = child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(key.as_bytes());
    let status = child
        .wait()
        .map_err(|_| "could not wait for Secret Service")?;
    if written.is_err() || !status.success() {
        return Err("Secret Service could not save the key. Unlock your desktop keyring or use OPENAI_API_KEY.".into());
    }
    Ok(())
}

pub fn logout() -> Result<(), String> {
    let status = keyring("clear")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .status()
        .map_err(|_| "could not start secret-tool")?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "GiCo could not clear its OpenAI keyring entry".into())
}
