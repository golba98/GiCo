//! Explicit user approval. Generated commands are NEVER executed
//! automatically; execution happens only after the user explicitly chooses
//! to run the exact verified command.

use std::io::Write;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Run this exact command string via the user's shell.
    Run(String),
    /// Open the command in $EDITOR for manual editing.
    Edit,
    /// Regenerate with corrected/fresh context.
    Regenerate,
    /// Do not run.
    Cancel,
    /// Non-interactive session: just display, never execute.
    ShowOnly,
}

/// Pure mapping from a user's typed answer to a decision. Testable.
pub fn decide(input: &str) -> Decision {
    match input.trim().to_lowercase().as_str() {
        "r" | "run" | "y" | "yes" => Decision::Run(String::new()), // command filled by caller
        "e" | "edit" => Decision::Edit,
        "g" | "regen" | "regenerate" => Decision::Regenerate,
        "c" | "cancel" | "n" | "no" | "" | "q" | "quit" => Decision::Cancel,
        _ => Decision::Cancel,
    }
}

/// Ask the user what to do with `command`. Returns ShowOnly when stdin is
/// not a TTY (nothing is ever executed in that case).
pub fn ask(command: &str) -> Decision {
    if !is_tty() {
        return Decision::ShowOnly;
    }
    print!("Run this command? [r]un / [e]dit / re[g]enerate / [c]ancel: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return Decision::ShowOnly;
    }
    match decide(&line) {
        Decision::Run(_) => Decision::Run(command.to_string()),
        other => other,
    }
}

fn is_tty() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: isatty on stdin.
        unsafe { libc_isatty(0) == 1 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(unix)]
unsafe fn libc_isatty(fd: i32) -> i32 {
    extern "C" {
        fn isatty(fd: i32) -> i32;
    }
    isatty(fd)
}

/// Execute `command` in the user's shell with inherited stdio (so stdout,
/// stderr, exit status and interactive input all behave natively).
/// Only ever called with a command the user explicitly approved.
///
/// While the child runs, SIGINT is ignored in this parent process so Ctrl+C
/// reaches the child (same foreground process group) and we can still reap
/// it and propagate its real exit status.
pub fn execute(command: &str) -> std::io::Result<std::process::ExitStatus> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
    }
    let status = std::process::Command::new(shell)
        .arg("-c")
        .arg(command)
        .status();
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
    }
    status
}

/// Like [`execute`], but additionally pipes the child's stderr through a tee
/// thread that streams it to our stderr unchanged while retaining the last
/// few KB. Returns the exit status plus that stderr tail, so a failed run can
/// be fed back into a user-requested repair round. stdout and stdin stay
/// inherited, so output still streams live and interactive programs work.
pub fn execute_capture_tail(command: &str) -> std::io::Result<(std::process::ExitStatus, String)> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
    }
    let mut child = std::process::Command::new(shell)
        .arg("-c")
        .arg(command)
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let mut err = child.stderr.take().expect("piped stderr");
    let tee = std::thread::spawn(move || {
        use std::io::Read;
        let mut tail: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match err.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = std::io::stderr().write_all(&buf[..n]);
                    let _ = std::io::stderr().flush();
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > 4096 {
                        tail.drain(..tail.len() - 4096);
                    }
                }
            }
        }
        String::from_utf8_lossy(&tail).into_owned()
    });
    let status = child.wait()?;
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
    }
    let tail = tee.join().unwrap_or_default();
    Ok((status, tail))
}

/// Open `command` in $EDITOR and return the edited text.
pub fn edit_command(command: &str) -> std::io::Result<String> {
    let dir = std::env::temp_dir();
    let path = dir.join("gico-edit.sh");
    std::fs::write(&path, command)?;
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let status = std::process::Command::new(&editor).arg(&path).status()?;
    if !status.success() {
        return Err(std::io::Error::other("editor exited non-zero"));
    }
    Ok(std::fs::read_to_string(&path)?.trim().to_string())
}
