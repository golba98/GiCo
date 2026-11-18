use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(super) enum Readiness {
    Window(Option<String>),
    Listener(Option<u16>),
}

pub(super) struct Observer {
    expectation: Readiness,
    next: Instant,
    seen_process: bool,
    result: Option<String>,
}

impl Observer {
    pub(super) fn new(expectation: Readiness) -> Self {
        Self {
            expectation,
            next: Instant::now(),
            seen_process: false,
            result: None,
        }
    }

    pub(super) fn check(&mut self, group: libc::pid_t) {
        if self.result.is_some() || Instant::now() < self.next {
            return;
        }
        self.next = Instant::now() + Duration::from_millis(500);
        let members = std::fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .take(4096)
            .flatten()
            .filter_map(|entry| {
                let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
                let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
                let pgid = stat
                    .rsplit_once(')')?
                    .1
                    .split_whitespace()
                    .nth(2)?
                    .parse::<i32>()
                    .ok()?;
                (pgid == group).then_some(pid)
            })
            .collect::<Vec<_>>();
        match &self.expectation {
            Readiness::Window(name) => {
                let candidates = members
                    .into_iter()
                    .filter(|pid| {
                        name.as_ref().is_none_or(|name| {
                            std::fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|p| {
                                p.file_name()
                                    .is_some_and(|p| p == std::ffi::OsStr::new(name))
                            })
                        })
                    })
                    .collect::<Vec<_>>();
                self.seen_process |= !candidates.is_empty();
                if candidates.is_empty() || std::env::var_os("DISPLAY").is_none() {
                    return;
                }
                let windows = std::process::Command::new("/usr/bin/timeout")
                    .args(["0.4", "/usr/bin/wmctrl", "-lp"])
                    .env_remove("OPENAI_API_KEY")
                    .output();
                if windows.is_ok_and(|out| {
                    out.status.success()
                        && String::from_utf8_lossy(&out.stdout).lines().any(|l| {
                            l.split_whitespace()
                                .nth(2)
                                .and_then(|s| s.parse::<u32>().ok())
                                .is_some_and(|pid| candidates.contains(&pid))
                        })
                }) {
                    self.ready("GUI window opened".into());
                }
            }
            Readiness::Listener(expected) => {
                if let Some(port) = listener_in_group(&members, *expected) {
                    self.ready(format!("Listening on local port {port}"));
                }
            }
        }
    }

    fn ready(&mut self, result: String) {
        eprintln!("gico: {result}");
        self.result = Some(result);
    }

    pub(super) fn finish(self) -> (bool, String) {
        match self.result {
            Some(result) => (true, result),
            None if matches!(self.expectation, Readiness::Window(_)) && self.seen_process => {
                (false, "process started, window unverified".into())
            }
            None => (false, "requested application readiness unverified".into()),
        }
    }
}

fn listener_in_group(members: &[u32], expected: Option<u16>) -> Option<u16> {
    let mut sockets = std::collections::HashMap::new();
    for file in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(table) = std::fs::read_to_string(file) else {
            continue;
        };
        for line in table.lines().skip(1).take(4096) {
            let row: Vec<_> = line.split_whitespace().collect();
            if row.len() < 10 || row[3] != "0A" {
                continue;
            }
            let Some((host, port)) = row[1].split_once(':') else {
                continue;
            };
            if !matches!(
                host,
                "00000000"
                    | "0100007F"
                    | "00000000000000000000000000000000"
                    | "00000000000000000000000001000000"
            ) {
                continue;
            }
            let Ok(port) = u16::from_str_radix(port, 16) else {
                continue;
            };
            if expected.is_none_or(|e| e == port) {
                sockets.insert(format!("socket:[{}]", row[9]), port);
            }
        }
    }
    members.iter().find_map(|pid| {
        Path::new(&format!("/proc/{pid}/fd"))
            .read_dir()
            .ok()?
            .take(128)
            .flatten()
            .find_map(|entry| {
                let link = entry.path().read_link().ok()?;
                sockets.get(link.to_str()?).copied()
            })
    })
}
