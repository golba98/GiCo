use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::CancelToken;

#[allow(dead_code)]
#[path = "../vendor/codex_head_tail_buffer.rs"]
mod buffer;
#[path = "../vendor/codex_process_group.rs"]
mod group;

use buffer::HeadTailBuffer;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecOutcome {
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub stdout_omitted_bytes: usize,
    pub stderr_omitted_bytes: usize,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub killed: bool,
    pub started: bool,
    pub cancelled: bool,
}

impl ExecOutcome {
    pub fn shell_code(&self) -> i32 {
        if self.timed_out {
            124
        } else if self.cancelled {
            130
        } else {
            self.exit_code
                .or_else(|| self.signal.map(|signal| 128 + signal))
                .unwrap_or(2)
        }
    }
}

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::Relaxed);
}

/// Scoped SIGINT handler for the CLI. Child processes restore the default at exec.
pub struct InterruptGuard(libc::sigaction);

impl InterruptGuard {
    pub fn install() -> io::Result<Self> {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = handle_interrupt as *const () as libc::sighandler_t;
        INTERRUPTED.store(false, Ordering::Relaxed);
        let result = unsafe {
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(libc::SIGINT, &action, &mut previous)
        };
        syscall(result)?;
        Ok(Self(previous))
    }

    pub(super) fn received() -> bool {
        INTERRUPTED.load(Ordering::Relaxed)
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        unsafe { libc::sigaction(libc::SIGINT, &self.0, std::ptr::null_mut()) };
        INTERRUPTED.store(false, Ordering::Relaxed);
    }
}

struct Foreground(libc::pid_t);

impl Foreground {
    fn current() -> Option<Self> {
        if !std::io::stdin().is_terminal() {
            return None;
        }
        let parent_group = unsafe { libc::getpgrp() };
        (unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) } == parent_group)
            .then_some(Self(parent_group))
    }
}

impl Drop for Foreground {
    fn drop(&mut self) {
        let _ = set_foreground(self.0);
    }
}

fn syscall(result: libc::c_int) -> io::Result<()> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// Block SIGTTOU only for the terminal handoff, including when reclaiming it
// from a foreground child. This also runs after fork, before the child's exec.
fn set_foreground(group: libc::pid_t) -> io::Result<()> {
    unsafe {
        let mut mask = std::mem::zeroed();
        let mut previous = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGTTOU);
        syscall(libc::sigprocmask(libc::SIG_BLOCK, &mask, &mut previous))?;
        let result = syscall(libc::tcsetpgrp(libc::STDIN_FILENO, group));
        let restored = syscall(libc::sigprocmask(
            libc::SIG_SETMASK,
            &previous,
            std::ptr::null_mut(),
        ));
        result.and(restored)
    }
}

struct Capture<R> {
    pipe: R,
    bytes: HeadTailBuffer<4096>,
    eof: bool,
}

impl<R: Read + AsRawFd> Capture<R> {
    fn new(pipe: R) -> Self {
        Self {
            pipe,
            bytes: HeadTailBuffer::default(),
            eof: false,
        }
    }

    fn set_nonblocking(&self) -> io::Result<()> {
        let fd: RawFd = self.pipe.as_raw_fd();
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            syscall(flags)?;
            syscall(libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK))
        }
    }

    fn drain(&mut self, mut sink: impl Write) -> io::Result<bool> {
        let mut progress = false;
        let mut chunk = [0; 8192];
        for _ in 0..8 {
            if self.eof {
                break;
            }
            match self.pipe.read(&mut chunk) {
                Ok(0) => self.eof = true,
                Ok(n) => {
                    self.bytes.push_chunk(&chunk[..n]);
                    sink.write_all(&chunk[..n])?;
                    progress = true;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        if progress {
            sink.flush()?;
        }
        Ok(progress)
    }

    fn preview(&self) -> String {
        String::from_utf8_lossy(&self.bytes.to_bytes_with_omission_marker()).into_owned()
    }
}

/// Execute an approved command with a dedicated process group and live output.
pub fn exec_shell(
    command: &str,
    cwd: &Path,
    cancel: &CancelToken,
    timeout: Duration,
) -> io::Result<ExecOutcome> {
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let mut builder = Command::new(shell);
    builder.arg("-c").arg(command);
    exec_process(builder, cwd, cancel, timeout)
}

pub(super) fn exec_process(
    mut command_builder: Command,
    cwd: &Path,
    cancel: &CancelToken,
    timeout: Duration,
) -> io::Result<ExecOutcome> {
    if cancel.is_cancelled() {
        return Ok(outcome(
            None,
            true,
            false,
            false,
            String::new(),
            String::new(),
            (0, 0),
        ));
    }
    command_builder.env_remove("OPENAI_API_KEY");
    let foreground = Foreground::current();
    let handoff = foreground.is_some();
    command_builder
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The handoff happens before exec so a command that reads immediately
    // cannot be stopped by SIGTTIN while waiting for its parent.
    unsafe {
        command_builder.pre_exec(move || {
            group::set_process_group()?;
            if handoff {
                set_foreground(libc::getpgrp())?;
            }
            Ok(())
        });
    }

    let mut child = command_builder.spawn()?;
    let pgid = child.id() as libc::pid_t;
    let mut stdout = Capture::new(child.stdout.take().expect("piped stdout"));
    let mut stderr = Capture::new(child.stderr.take().expect("piped stderr"));
    let start = Instant::now();
    let mut reaped = false;
    let result = (|| {
        stdout.set_nonblocking()?;
        stderr.set_nonblocking()?;
        let (status, cancelled, timed_out) = loop {
            let cancelled = cancel.is_cancelled();
            let timed_out = !cancelled && !timeout.is_zero() && start.elapsed() >= timeout;
            if cancelled || timed_out {
                group::signal_process_group_id(pgid, libc::SIGKILL)?;
                let status = child.wait()?;
                reaped = true;
                break (status, cancelled, timed_out);
            }
            if let Some(status) = child.try_wait()? {
                reaped = true;
                break (status, false, false);
            }
            let progressed = stdout.drain(io::stdout())? | stderr.drain(io::stderr())?;
            if !progressed {
                std::thread::sleep(Duration::from_millis(10));
            }
        };

        // Descendants can inherit these pipes past the shell's exit.
        let drain_start = Instant::now();
        while drain_start.elapsed() < Duration::from_millis(50) {
            if !(stdout.drain(io::stdout())? | stderr.drain(io::stderr())?) {
                break;
            }
        }
        Ok(outcome(
            Some(status),
            cancelled,
            timed_out,
            true,
            stdout.preview(),
            stderr.preview(),
            (stdout.bytes.omitted_bytes(), stderr.bytes.omitted_bytes()),
        ))
    })();
    if !reaped {
        let _ = group::signal_process_group_id(pgid, libc::SIGKILL);
        let _ = child.kill();
        let _ = child.wait();
    }
    drop(foreground);
    result
}

fn outcome(
    status: Option<ExitStatus>,
    cancelled: bool,
    timed_out: bool,
    started: bool,
    stdout_tail: String,
    stderr_tail: String,
    omitted: (usize, usize),
) -> ExecOutcome {
    let signal = status.and_then(|status| status.signal());
    ExecOutcome {
        exit_code: status.and_then(|status| status.code()),
        success: status.is_some_and(|status| status.success()) && !cancelled && !timed_out,
        stdout_tail,
        stderr_tail,
        stdout_omitted_bytes: omitted.0,
        stderr_omitted_bytes: omitted.1,
        signal,
        timed_out,
        killed: (started && (cancelled || timed_out)) || signal.is_some(),
        started,
        cancelled,
    }
}
