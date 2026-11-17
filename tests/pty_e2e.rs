//! End-to-end PTY tests: run the real gico binary in a pseudo-terminal with a
//! mocked model output (GICO_MOCK_COMMAND) and drive it with real keystrokes.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize};

fn gico_bin() -> &'static str {
    env!("CARGO_BIN_EXE_gico")
}

struct PtySession {
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    rx: mpsc::Receiver<u8>,
}

fn spawn_gico(dir: &Path, mock_cmd: &str) -> PtySession {
    spawn_gico_with(dir, "do the thing", mock_cmd)
}

fn spawn_pty(cmd: CommandBuilder) -> PtySession {
    spawn_pty_at_width(cmd, 100)
}

fn spawn_pty_at_width(cmd: CommandBuilder, cols: u16) -> PtySession {
    let pair = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let child = pair.slave.spawn_command(cmd).expect("spawn");
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().expect("reader");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    for b in &buf[..n] {
                        if tx.send(*b).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });
    let writer = pair.master.take_writer().expect("writer");
    PtySession { writer, child, rx }
}

impl PtySession {
    fn send(&mut self, s: &str) {
        self.writer.write_all(s.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    /// Collect output until `needle` appears or timeout elapses.
    fn read_until(&self, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(b) => {
                    out.push(b);
                    if String::from_utf8_lossy(&out).contains(needle) {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Collect output for a fixed duration.
    fn read_for(&self, dur: Duration) -> String {
        let deadline = Instant::now() + dur;
        let mut out = Vec::new();
        while Instant::now() < deadline {
            if let Ok(b) = self.rx.recv_timeout(Duration::from_millis(50)) {
                out.push(b);
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn wait(&mut self) -> Option<i32> {
        let mut killer = self.child.clone_killer();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status.exit_code() as i32),
                Ok(None) => {}
                Err(_) => return None,
            }
            if Instant::now() > deadline {
                let _ = killer.kill();
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn fixture_dir() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("main.cpp"), "int main(){}\n").unwrap();
    tmp
}

#[test]
fn pty_enter_executes_valid_command_and_propagates_exit_code() {
    let dir = fixture_dir();
    let mut s = spawn_gico(dir.path(), "echo PTY_RAN_MARKER; exit 3");
    let out = s.read_until("↵ run", Duration::from_secs(15));
    assert!(out.contains("gico ›"), "output: {out:?}");
    assert!(
        out.contains("echo PTY_RAN_MARKER; exit 3"),
        "output: {out:?}"
    );
    s.send("\r"); // Enter
    let out2 = s.read_until("PTY_RAN_MARKER", Duration::from_secs(15));
    assert!(
        out2.contains("PTY_RAN_MARKER"),
        "command output missing: {out2:?}"
    );
    // Non-zero exit: gico returns to the prompt for a repair round; cancelling
    // then propagates the command's real exit code.
    let out3 = s.read_until("exited with code 3", Duration::from_secs(15));
    assert!(
        out3.contains("exited with code 3"),
        "repair note missing: {out3:?}"
    );
    let out4 = s.read_until("c cancel", Duration::from_secs(15));
    assert!(
        out4.contains("r refine"),
        "repair controls missing: {out4:?}"
    );
    s.send("c");
    let code = s.wait();
    assert_eq!(code, Some(3), "exit code must propagate");
}

#[test]
fn pty_c_cancels_without_executing() {
    let dir = fixture_dir();
    let mut s = spawn_gico(dir.path(), "echo SHOULD_NOT_RUN");
    let out = s.read_until("c cancel", Duration::from_secs(15));
    assert!(out.contains("gico ›"));
    s.send("c");
    let code = s.wait();
    assert_eq!(code, Some(0));
    // the mocked command must never have run
    let late = s.read_for(Duration::from_secs(1));
    assert!(
        !late.contains("SHOULD_NOT_RUN"),
        "command ran after cancel: {late:?}"
    );
}

#[test]
fn pty_escape_cancels() {
    let dir = fixture_dir();
    let mut s = spawn_gico(dir.path(), "echo NOPE_ESC");
    s.read_until("c cancel", Duration::from_secs(15));
    s.send("\x1b"); // Escape
    assert_eq!(s.wait(), Some(0));
}

#[test]
fn pty_enter_cannot_execute_blocked_command() {
    let dir = fixture_dir();
    // references a path that does not exist -> verifier blocks
    let mut s = spawn_gico(dir.path(), "cat /definitely/not/here.txt");
    let out = s.read_until("blocked:", Duration::from_secs(15));
    assert!(out.contains("blocked:"), "output: {out:?}");
    assert!(
        !out.contains("↵ run"),
        "blocked suggestion must not offer run: {out:?}"
    );
    // Press Enter multiple times: must never execute.
    s.send("\r");
    std::thread::sleep(Duration::from_millis(500));
    s.send("\r");
    let out2 = s.read_until("blocked", Duration::from_secs(5));
    assert!(
        !out2.contains("No such file") || out2.contains("blocked"),
        "cat must not have run: {out2:?}"
    );
    // Cancel cleanly.
    s.send("c");
    assert_eq!(s.wait(), Some(0));
}

#[test]
fn pty_r_refine_and_e_edit_prompts_appear() {
    let dir = fixture_dir();
    let mut s = spawn_gico(dir.path(), "echo base");
    s.read_until("↵ run", Duration::from_secs(15));
    s.send("r");
    let out = s.read_until("Refine ›", Duration::from_secs(5));
    assert!(out.contains("Refine ›"), "out: {out:?}");
    s.send("make it louder\r");
    // mock returns the same command; suggestion reappears
    let out2 = s.read_until("↵ run", Duration::from_secs(15));
    assert!(out2.contains("gico ›"), "out2: {out2:?}");
    s.send("e");
    let out3 = s.read_until("Edit › echo base", Duration::from_secs(5));
    assert!(
        out3.contains("Edit › echo base"),
        "edit line must be prefilled: {out3:?}"
    );
    // abort edit with Escape, then cancel
    s.send("\x1b");
    std::thread::sleep(Duration::from_millis(300));
    s.send("c");
    assert_eq!(s.wait(), Some(0));
}

#[test]
fn pty_empty_project_is_informational_no_controls() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("build/src")).unwrap();
    std::fs::create_dir_all(tmp.path().join("build/tests")).unwrap();
    // request mentions compile -> short-circuit before model call
    let pair = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(gico_bin());
    cmd.arg("--no-cache");
    cmd.arg("Compile and run the C++ project in this directory");
    cmd.env("NO_COLOR", "1");
    cmd.env("GICO_MOCK_COMMAND", "cd ."); // must NOT be used
    cmd.cwd(tmp.path());
    let child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    for b in &buf[..n] {
                        if tx.send(*b).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut bytes = Vec::new();
    while Instant::now() < deadline {
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(100)) {
            bytes.push(b);
            if String::from_utf8_lossy(&bytes).contains("build/") {
                break;
            }
        }
    }
    let out = String::from_utf8_lossy(&bytes).into_owned();
    assert!(
        out.contains("No buildable C++ project found"),
        "out: {out:?}"
    );
    assert!(out.contains("build/"), "out: {out:?}");
    assert!(
        !out.contains("↵ run"),
        "informational response must not show controls: {out:?}"
    );
    // process should exit on its own
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut code = None;
    let mut child = child;
    loop {
        if let Ok(Some(st)) = child.try_wait() {
            code = Some(st.exit_code() as i32);
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(code, Some(0));
}

#[test]
fn pty_intent_mismatch_is_blocked_and_enter_never_runs_it() {
    // A shell source has no build detector, so the mock reaches intent
    // validation regardless of the compilers installed on the host.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("main.sh"), "echo hello\n").unwrap();
    let dir = tmp.path().to_path_buf();
    // Mock model insists on an irrelevant `cd` command for a build request.
    let mut s = spawn_gico_with(&dir, "compile and run this project", "cd .");
    let out = s.read_until("irrelevant to the request", Duration::from_secs(20));
    assert!(out.contains("blocked:"), "out: {out:?}");
    assert!(out.contains("irrelevant to the request"), "out: {out:?}");
    assert!(
        !out.contains("↵ run"),
        "blocked suggestion must not offer run: {out:?}"
    );
    // Enter must not execute; then cancel.
    s.send("\r");
    std::thread::sleep(Duration::from_millis(400));
    s.send("c");
    assert_eq!(s.wait(), Some(0));
}

fn spawn_gico_with(dir: &Path, request: &str, mock_cmd: &str) -> PtySession {
    let mut cmd = CommandBuilder::new(gico_bin());
    cmd.arg("--no-cache");
    cmd.arg(request);
    cmd.env("GICO_MOCK_COMMAND", mock_cmd);
    cmd.env("NO_COLOR", "1");
    cmd.env_remove("GICO_MODEL");
    cmd.cwd(dir);
    cmd.env("SHELL", "/bin/sh");
    spawn_pty(cmd)
}

#[test]
fn pty_foreground_command_can_read_stdin() {
    let dir = fixture_dir();
    let mut session = spawn_gico(dir.path(), "echo READY; read value; echo received:$value");
    assert!(session
        .read_until("↵ run", Duration::from_secs(5))
        .contains("↵ run"));
    session.send("\r");
    assert!(session
        .read_until("READY", Duration::from_secs(5))
        .contains("READY"));
    session.send("hello\r");
    assert!(session
        .read_until("received:hello", Duration::from_secs(5))
        .contains("received:hello"));
    assert_eq!(session.wait(), Some(0));
}

#[test]
fn pty_interrupt_reaches_the_approved_foreground_group() {
    let dir = fixture_dir();
    let mut session = spawn_gico(dir.path(), "echo READY; sleep 30");
    assert!(session
        .read_until("↵ run", Duration::from_secs(5))
        .contains("↵ run"));
    session.send("\r");
    assert!(session
        .read_until("READY", Duration::from_secs(5))
        .contains("READY"));
    session.send("\x03");
    assert_eq!(session.wait(), Some(130));
}

#[test]
fn pty_approval_rechecks_a_removed_path() {
    let dir = fixture_dir();
    let path = dir.path().join("sample.txt");
    std::fs::write(&path, "FILE_BODY").unwrap();
    let mut session = spawn_gico(dir.path(), "cat sample.txt");
    assert!(session
        .read_until("↵ run", Duration::from_secs(5))
        .contains("↵ run"));
    std::fs::remove_file(path).unwrap();
    session.send("\r");
    let output = session.read_until("does not exist", Duration::from_secs(5));
    assert!(output.contains("does not exist"), "{output}");
    session.send("c");
    assert_eq!(session.wait(), Some(0));
}

#[test]
fn pty_zsh_hook_applies_a_literal_directory_in_the_calling_shell() {
    let Ok(zsh) = std::process::Command::new("zsh").arg("--version").output() else {
        return;
    };
    assert!(zsh.status.success());
    let home = tempfile::tempdir().unwrap();
    let start = home.path().join("start");
    let target = home.path().join("Development/9-Rust 'work';$data");
    std::fs::create_dir(&start).unwrap();
    std::fs::create_dir_all(target.join("project")).unwrap();
    std::fs::write(target.join("project/Cargo.toml"), "[workspace]").unwrap();
    let prefix = Path::new(gico_bin()).parent().unwrap();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(prefix.to_owned()).chain(std::env::split_paths(&path)),
    )
    .unwrap();
    let mut cmd = CommandBuilder::new("zsh");
    cmd.args([
        "-f",
        "-c",
        r#"eval "$(command gico init-zsh)"
gico --no-cache "change to my Rust development directory"
rc=$?
printf 'shell-status=%s cwd=%s\n' "$rc" "$PWD"
exit "$rc""#,
    ]);
    cmd.env("HOME", home.path());
    cmd.env("TMPDIR", &start);
    cmd.env("PATH", path);
    cmd.env("NO_COLOR", "1");
    cmd.cwd(&start);
    let mut session = spawn_pty(cmd);
    let output = session.read_until("↵ run", Duration::from_secs(5));
    assert!(output.contains("9-Rust"), "{output}");
    session.send("\r");
    let expected = format!("shell-status=0 cwd={}", target.display());
    let output = session.read_until(&expected, Duration::from_secs(5));
    assert!(output.contains(&expected), "{output}");
    assert_eq!(session.wait(), Some(0));
    assert_eq!(std::fs::read_dir(start).unwrap().count(), 0);
}

#[test]
fn pty_timeout_exits_with_124() {
    let dir = fixture_dir();
    let mut cmd = CommandBuilder::new(gico_bin());
    cmd.args(["--no-cache", "--exec-timeout", "1", "do the thing"]);
    cmd.env("GICO_MOCK_COMMAND", "sleep 30");
    cmd.env("NO_COLOR", "1");
    cmd.env("SHELL", "/bin/sh");
    cmd.cwd(dir.path());
    let mut session = spawn_pty(cmd);
    assert!(session
        .read_until("↵ run", Duration::from_secs(5))
        .contains("↵ run"));
    session.send("\r");
    assert!(session
        .read_until("exit 124", Duration::from_secs(5))
        .contains("exit 124"));
    assert_eq!(session.wait(), Some(124));
}

#[test]
fn pty_ctrl_c_cancels_an_in_flight_model_request() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (sent, received) = mpsc::channel();
    let backend = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut chunk = [0; 4096];
        let count = stream.read(&mut chunk).unwrap();
        assert!(String::from_utf8_lossy(&chunk[..count]).contains("POST /api/chat"));
        sent.send(()).unwrap();
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
                Err(error) => panic!("HTTP connection remained open: {error}"),
            }
        }
    });
    let dir = fixture_dir();
    let mut cmd = CommandBuilder::new(gico_bin());
    cmd.args([
        "--no-cache",
        "--endpoint",
        &endpoint,
        "--model",
        "fixture",
        "do the thing",
    ]);
    cmd.env_remove("GICO_MOCK_COMMAND");
    cmd.env("NO_COLOR", "1");
    cmd.cwd(dir.path());
    let mut session = spawn_pty(cmd);
    received.recv_timeout(Duration::from_secs(5)).unwrap();
    session.send("\x03");
    assert_eq!(session.wait(), Some(130));
    backend.join().unwrap();
}

#[test]
fn pty_large_utf8_output_survives_capture_truncation() {
    let dir = fixture_dir();
    std::fs::write(
        dir.path().join("emit.py"),
        "import sys\nsys.stderr.write('ø🙃' * 920 + '\\nOUTPUT_END\\n')",
    )
    .unwrap();
    let mut session = spawn_gico(dir.path(), "python3 emit.py");
    assert!(session
        .read_until("↵ run", Duration::from_secs(5))
        .contains("↵ run"));
    session.send("\r");
    let output = session.read_until("OUTPUT_END", Duration::from_secs(5));
    assert!(output.contains("ø🙃") && output.contains("OUTPUT_END"));
    assert_eq!(session.wait(), Some(0));
}

fn cmake_ui_fixture(dir: &Path) {
    fn write(dir: &Path, path: &str, content: &str) {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    write(
        dir,
        "CMakeLists.txt",
        "project(app)\nadd_subdirectory(src/gui)\n",
    );
    write(dir, "src/gui/CMakeLists.txt", "add_executable(it-pixel-gui main.cpp)\ntarget_link_libraries(it-pixel-gui PRIVATE Qt6::Widgets)\n");
    write(dir, "src/gui/main.cpp", "int main() {}");
    for name in ["build", "build-debug"] {
        write(
            dir,
            &format!("{name}/CMakeCache.txt"),
            "CMAKE_HOME_DIRECTORY:INTERNAL=/old/pixel/project\n",
        );
    }
    write(
        dir,
        "build-gico-test/CMakeCache.txt",
        &format!("CMAKE_HOME_DIRECTORY:INTERNAL={}\n", dir.display()),
    );
    let executable = "build-gico-test/src/gui/it-pixel-gui";
    write(dir, executable, "#!/bin/sh\nexit 0\n");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.join(executable), std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn pty_gui_diagnostics_start_at_the_left_in_default_and_verbose_modes() {
    let fixture = tempfile::tempdir().unwrap();
    cmake_ui_fixture(fixture.path());
    for (flag, color) in [
        (None, false),
        (Some("--verbose"), true),
        (Some("--debug"), false),
    ] {
        let mut cmd = CommandBuilder::new(gico_bin());
        cmd.arg("--no-cache");
        if let Some(flag) = flag {
            cmd.arg(flag);
        }
        cmd.arg("find the graphical application built by this project and launch it");
        if color {
            cmd.env_remove("NO_COLOR");
        } else {
            cmd.env("NO_COLOR", "");
        }
        cmd.cwd(fixture.path());
        let mut session = spawn_pty_at_width(cmd, 80);
        let output = session.read_until("c cancel", Duration::from_secs(5));
        assert!(!output.replace("\r\n", "").contains('\n'), "{output:?}");
        assert_eq!(output.contains("\x1b[36m"), color, "{output:?}");
        if !color {
            assert!(!output.contains('\x1b'), "{output:?}");
        }
        let output = gico::ui::strip_ansi(&output);
        assert!(output.contains("\r  gico › ./build-gico-test/src/gui/it-pixel-gui\r\n         • Skipped stale builds: build/, build-debug/\r\n         • GUI application · Display server required\r\n\r\n         ↵ run · r refine · e edit · c cancel"), "{output:?}");
        assert_eq!(
            output.contains("cache was configured for '/old/pixel/project'"),
            flag.is_some()
        );
        session.send("c");
        assert_eq!(session.wait(), Some(0));
    }
}

fn editor_frame(rendered: &str) -> (&str, usize) {
    let last = rendered.rsplit("\x1b[2K").next().unwrap();
    let (line, position) = last.rsplit_once("\x1b[").unwrap();
    let column = position.strip_suffix('G').unwrap().parse().unwrap();
    (line, column)
}

#[test]
fn pty_unicode_edit_uses_one_visible_row_and_moves_by_columns() {
    let dir = fixture_dir();
    let command = "echo 'a long path with spaces and 文e\u{301}🙂'";
    let mut cmd = CommandBuilder::new(gico_bin());
    cmd.args(["--no-cache", "do the thing"]);
    cmd.env("GICO_MOCK_COMMAND", command);
    cmd.env("NO_COLOR", "1");
    cmd.cwd(dir.path());
    let mut session = spawn_pty_at_width(cmd, 26);
    let preview = session.read_until("c cancel", Duration::from_secs(5));
    assert!(preview
        .lines()
        .all(|line| gico::ui::display_width(line.trim_matches('\r')) <= 26));
    session.send("e");
    let frame = session.read_for(Duration::from_millis(350));
    let frame = frame.trim_start_matches("\r\n");
    assert!(!frame.contains('\n'), "{frame:?}");
    let (line, end) = editor_frame(frame);
    assert!(line.starts_with("Edit › …"), "{frame:?}");
    assert!(line.ends_with("文e\u{301}🙂'"), "{frame:?}");
    let width = gico::ui::display_width(line);
    assert!(width < 26);
    assert_eq!(end, width + 1);

    session.send("\x1b[D\x1b[D");
    let frame = session.read_for(Duration::from_millis(350));
    let (_, before_emoji) = editor_frame(&frame);
    assert_eq!(before_emoji, end - 3, "{frame:?}");

    session.send("\x1b[D");
    let frame = session.read_for(Duration::from_millis(350));
    let (_, before_accent) = editor_frame(&frame);
    assert_eq!(before_accent, before_emoji - 1, "{frame:?}");

    session.send("\x1b[H");
    let frame = session.read_for(Duration::from_millis(350));
    let (line, home) = editor_frame(&frame);
    assert!(
        line.starts_with("Edit › echo ") && line.ends_with('…'),
        "{frame:?}"
    );
    assert_eq!(home, gico::ui::display_width("Edit › ") + 1);
    session.send("Z\r");
    let edited = session.read_until("c cancel", Duration::from_secs(5));
    assert!(edited.contains("Zecho"), "{edited:?}");
    session.send("c");
    assert_eq!(session.wait(), Some(0));
}

#[test]
fn pty_restores_terminal_modes_after_cancel_and_before_execution() {
    let dir = fixture_dir();
    for (answer, ran) in [("c", false), ("\x1b", false), ("\r", true)] {
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args([
            "-c",
            "before=$(stty -g) || exit 1; export BEFORE=\"$before\"; \"$@\"; rc=$?; test \"$before\" = \"$(stty -g)\" || exit 2; printf 'TTY_RESTORED\\n'; exit \"$rc\"",
            "fixture", gico_bin(), "--no-cache", "do the thing",
        ]);
        cmd.env("SHELL", "/bin/sh");
        cmd.env("NO_COLOR", "1");
        cmd.env(
            "GICO_MOCK_COMMAND",
            "test \"$BEFORE\" = $(stty -g) && echo TTY_COOKED",
        );
        cmd.cwd(dir.path());
        let mut session = spawn_pty(cmd);
        let preview = session.read_until("c cancel", Duration::from_secs(5));
        assert!(preview.contains("↵ run"), "{preview:?}");
        session.send(answer);
        let output = session.read_until("TTY_RESTORED", Duration::from_secs(5));
        assert!(output.contains("TTY_RESTORED"), "{output:?}");
        assert_eq!(output.contains("\r\nTTY_COOKED\r\n"), ran, "{output:?}");
        assert_eq!(session.wait(), Some(0));
    }
}
