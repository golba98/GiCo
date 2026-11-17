//! gico - context-aware local AI terminal assistant.
//!
//! Minimal interface:
//!   gico "find all cpp files"
//!     gico › find . -type f -name '*.cpp'
//!            ↵ run · r refine · e edit · c cancel

use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use gico::context::{ContextEngine, EngineConfig, RawScan};
use gico::harness::pipeline::{self, Candidate, Dispatch, Preparation};
use gico::harness::{CancelToken, Harness, InterruptGuard, OpKind, OpOutcome};
use gico::llm::{Backend, LlmClient};
use gico::verify::{self, Report, Verdict};
use gico::{interact, ui};

#[derive(Parser)]
#[command(
    name = "gico",
    version,
    about = "Context-aware local AI terminal assistant"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// The natural-language request (when no subcommand is given).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    request: Vec<String>,

    /// Model name (overrides GICO_MODEL and config file). Never hard-coded.
    #[arg(long)]
    model: Option<String>,

    /// Inference backend: ollama | llamacpp (OpenAI-compatible).
    #[arg(long)]
    backend: Option<String>,

    /// Endpoint URL (overrides GICO_ENDPOINT and config file).
    #[arg(long)]
    endpoint: Option<String>,

    /// Context token budget (approx tokens, chars/4).
    #[arg(long)]
    budget: Option<usize>,

    /// Disable context collection (baseline mode, useful for benchmarks).
    #[arg(long)]
    no_context: bool,

    /// Print the collected context and rendered prompt, do not call the model.
    #[arg(long)]
    dry_run: bool,

    /// Print collected context as JSON and exit.
    #[arg(long)]
    json: bool,

    /// Disable the context cache.
    #[arg(long)]
    no_cache: bool,

    /// Cache directory override.
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Sampling temperature.
    #[arg(long)]
    temperature: Option<f32>,

    /// Max tokens to generate.
    #[arg(long)]
    max_tokens: Option<u32>,

    /// Show context summary and diagnostics on stderr.
    #[arg(long)]
    verbose: bool,

    /// Show full rendered prompt and raw model output on stderr.
    #[arg(long)]
    debug: bool,

    /// Config file path (default ~/.config/gico/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,

    /// Kill the executed command's process group after this many seconds
    /// (0 = no timeout). Default 3600.
    #[arg(long)]
    exec_timeout: Option<u64>,

    /// Internal: when set, an approved pure `cd <dir>` writes the resolved
    /// target into this private directory (used by the Zsh hook).
    #[arg(long, hide = true)]
    cd_hook_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the collected context and exit.
    Context {
        /// Print collected context as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Validate a command against the current directory.
    Verify {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// List models available on the inference backend.
    Models,
    /// Print the opt-in zsh integration snippet (eval "$(gico init-zsh)").
    InitZsh,
}

#[derive(serde::Deserialize, Default)]
struct FileConfig {
    model: Option<String>,
    backend: Option<String>,
    endpoint: Option<String>,
    budget_tokens: Option<usize>,
}

fn load_config(path: Option<PathBuf>) -> FileConfig {
    let path = path.or_else(|| {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/gico/config.toml"))
    });
    let Some(p) = path else {
        return FileConfig::default();
    };
    let Ok(data) = std::fs::read_to_string(p) else {
        return FileConfig::default();
    };
    toml::from_str(&data).unwrap_or_default()
}

/// Runs the approved directory change in the calling Zsh process.
const ZSH_HOOK_SNIPPET: &str = r#"# GiCo shell integration (opt-in). Add to ~/.zshrc:
#   eval "$(gico init-zsh)"
gico() {
  emulate -L zsh
  local handoff target
  handoff=$(command mktemp -d -- "${TMPDIR:-/tmp}/gico-zsh.XXXXXXXX") || return
  {
    command gico --cd-hook-dir "$handoff" "$@" || return $?
    if [[ -f "$handoff/target" ]]; then
      IFS= read -r -d '' target < "$handoff/target" || return 2
      builtin cd -- "$target" || return $?
    fi
  } always {
    command rm -f -- "$handoff/target"
    command rmdir -- "$handoff"
  }
}
"#;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let cfg = load_config(cli.config.clone());

    let model = cli
        .model
        .clone()
        .or_else(|| std::env::var("GICO_MODEL").ok())
        .or(cfg.model.clone());
    let backend_s = cli
        .backend
        .clone()
        .or_else(|| std::env::var("GICO_BACKEND").ok())
        .or(cfg.backend.clone())
        .unwrap_or_else(|| "ollama".to_string());
    let endpoint = cli
        .endpoint
        .clone()
        .or_else(|| std::env::var("GICO_ENDPOINT").ok())
        .or(cfg.endpoint.clone())
        .unwrap_or_else(|| "http://127.0.0.1:11434".to_string());
    let budget = cli
        .budget
        .or_else(|| {
            std::env::var("GICO_BUDGET")
                .ok()
                .and_then(|v| v.parse().ok())
        })
        .or(cfg.budget_tokens)
        .unwrap_or(700);

    let engine_cfg = EngineConfig {
        budget_tokens: budget,
        cache_dir: if cli.no_cache {
            None
        } else {
            cli.cache_dir
                .clone()
                .or(Some(gico::context::default_cache_dir()))
        },
        ..Default::default()
    };
    let engine = ContextEngine::new(engine_cfg);

    match cli.command {
        Some(Cmd::Context { json }) => {
            let ctx = engine.collect("").unwrap_or_else(|e| {
                eprintln!("gico: error collecting context: {e}");
                std::process::exit(2);
            });
            if json || cli.json {
                println!("{}", serde_json::to_string_pretty(&ctx).unwrap());
            } else {
                print_context(&ctx);
            }
            ExitCode::SUCCESS
        }
        Some(Cmd::InitZsh) => {
            print!("{}", ZSH_HOOK_SNIPPET);
            ExitCode::SUCCESS
        }
        Some(Cmd::Models) => {
            let Some(backend) = Backend::parse(&backend_s) else {
                eprintln!("gico: unknown backend '{backend_s}' (use ollama or llamacpp)");
                return ExitCode::from(2);
            };
            let client = LlmClient::new(backend, &endpoint, &model.clone().unwrap_or_default());
            match client.list_models() {
                Ok(models) => {
                    for m in models {
                        println!("{m}");
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("gico: could not list models from {endpoint} ({e})");
                    ExitCode::from(2)
                }
            }
        }
        Some(Cmd::Verify { command }) => {
            let cmd = command.join(" ");
            let cwd = std::env::current_dir().unwrap();
            let scan = engine.scan(&cwd).unwrap();
            let report = verify::verify(&cmd, &cwd, &scan.entries);
            print_report(&report);
            match report.verdict() {
                Verdict::Blocked => ExitCode::from(1),
                _ => ExitCode::SUCCESS,
            }
        }
        None => {
            let request = cli.request.join(" ");
            if request.trim().is_empty() {
                eprintln!("usage: gico \"<request>\"  (or: gico context | gico verify <cmd> | gico models)");
                return ExitCode::from(2);
            }
            run_request(&engine, &request, &cli, &model, &backend_s, &endpoint)
        }
    }
}

fn print_context(ctx: &gico::context::Context) {
    let e = &ctx.environment;
    println!("OS: {} ({})", e.os, e.arch);
    println!("Shell: {}", e.shell);
    println!("CWD: {}", e.cwd.display());
    println!("Tools: {}", e.utilities.join(", "));
    println!("Compilers: {}", e.compilers.join(", "));
    println!("Package managers: {}", e.package_managers.join(", "));
    if let Some(g) = &ctx.git.branch {
        println!("Git branch: {g}");
    }
    match &ctx.project {
        Some(p) => {
            println!("Project: {}", p.kind);
            if let Some(b) = &p.build_system {
                println!("Build system: {b}");
            }
            if let Some(b) = &p.build_command {
                println!("Build command: {b}");
            }
            if let Some(r) = &p.run_command {
                println!("Run command: {r}");
            }
        }
        None => println!("Project: none detected"),
    }
    println!(
        "Files ({} of {} discovered):",
        ctx.files.len(),
        ctx.total_discovered
    );
    for f in &ctx.files {
        println!("  {}", f.path);
    }
    if ctx.excluded_sensitive > 0 {
        println!("  ({} sensitive entries withheld)", ctx.excluded_sensitive);
    }
    if ctx.excluded_injection > 0 {
        println!("  ({} suspicious entries withheld)", ctx.excluded_injection);
    }
}

fn print_report(report: &Report) {
    println!("command: {}", report.command);
    match report.verdict() {
        Verdict::Valid => println!("verdict: valid"),
        Verdict::Warning => println!("verdict: warning"),
        Verdict::Blocked => println!("verdict: blocked"),
    }
    for i in &report.issues {
        let tag = match i.severity {
            verify::Severity::Error => "ERROR",
            verify::Severity::Warning => "WARN",
            verify::Severity::Info => "INFO",
        };
        println!("[{tag}] {}", i.message);
        if let Some(s) = &i.suggestion {
            println!("       suggestion: {s}");
        }
    }
}

struct Generator<'a> {
    engine: &'a ContextEngine,
    cli: &'a Cli,
    backend: Option<Backend>,
    endpoint: &'a str,
    model: Option<&'a str>,
    client: std::cell::OnceCell<LlmClient>,
}

impl Generator<'_> {
    fn generate(
        &self,
        raw: &RawScan,
        request: &str,
        feedback: Option<(&str, &str)>,
        cancel: &CancelToken,
    ) -> Result<String, String> {
        if let Ok(mock) = std::env::var("GICO_MOCK_COMMAND") {
            return Ok(mock);
        }
        let backend = self.backend.ok_or("unknown backend")?;
        let model = self.model.ok_or("no model configured")?;
        let client = self.client.get_or_init(|| {
            let mut client = LlmClient::new(backend, self.endpoint, model);
            client.temperature = self.cli.temperature.unwrap_or(client.temperature);
            client.max_tokens = self.cli.max_tokens.unwrap_or(client.max_tokens);
            client
        });
        let prompt =
            pipeline::generation_prompt(self.engine, raw, request, feedback, self.cli.no_context);
        if self.cli.debug {
            eprintln!(
                "--- prompt ---\n{}--- end prompt ---",
                ui::safe_multiline(&prompt)
            );
        }
        let output = client
            .complete_cancellable(&prompt, cancel)
            .map_err(|error| error.to_string())?;
        if self.cli.debug {
            eprintln!(
                "--- raw model output ---\n{}\n--- end ---",
                ui::safe_multiline(&output)
            );
        }
        Ok(output)
    }
}

fn run_request(
    engine: &ContextEngine,
    request: &str,
    cli: &Cli,
    model: &Option<String>,
    backend_s: &str,
    endpoint: &str,
) -> ExitCode {
    let _interrupts = match InterruptGuard::install() {
        Ok(guard) => guard,
        Err(error) => return io_error("install cancellation handler", error),
    };
    let generator = Generator {
        engine,
        cli,
        backend: Backend::parse(backend_s),
        endpoint,
        model: model.as_deref(),
        client: std::cell::OnceCell::new(),
    };
    let mut harness = Harness::new(5);
    harness.enable_interrupts();
    let code = run_pipeline(&generator, request, &mut harness);
    if cli.verbose || cli.debug {
        for op in harness.ops() {
            eprintln!(
                "op {}: {} ({:?}, {} ms)",
                op.kind.as_str(),
                ui::safe_multiline(&op.detail),
                op.outcome,
                op.elapsed_ms,
            );
        }
    }
    ExitCode::from(code)
}

fn scan(
    engine: &ContextEngine,
    cwd: &std::path::Path,
    harness: &mut Harness,
) -> std::io::Result<RawScan> {
    let start = std::time::Instant::now();
    let result = engine.scan(cwd);
    let outcome = match &result {
        Ok(_) => OpOutcome::Ok,
        Err(error) => OpOutcome::Failed(error.to_string()),
    };
    harness.record_ms(
        OpKind::Inspect,
        "filesystem evidence",
        outcome,
        start.elapsed().as_millis(),
    );
    result
}

fn run_pipeline(generator: &Generator<'_>, request: &str, harness: &mut Harness) -> u8 {
    let cli = generator.cli;
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let (style, width) = if interactive {
        (ui::Style::detect(), interact::term_width())
    } else {
        (ui::Style::plain(), ui::DEFAULT_WIDTH)
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => return io_error("resolve working directory", error),
    };
    let raw = match scan(generator.engine, &cwd, harness) {
        Ok(raw) => raw,
        Err(error) => return io_error("inspect working directory", error),
    };
    if cli.json || cli.dry_run || cli.verbose || cli.debug {
        let ctx = generator.engine.context_from_scan(&raw, request);
        if cli.json {
            println!("{}", serde_json::to_string_pretty(&ctx).unwrap());
            return 0;
        }
        if cli.dry_run {
            print_context(&ctx);
            let prompt =
                pipeline::generation_prompt(generator.engine, &raw, request, None, cli.no_context);
            eprintln!("--- prompt ---\n{}", ui::safe_multiline(&prompt));
            return 0;
        }
        eprintln!(
            "context: {} files, project: {}, est. {} tokens",
            ctx.files.len(),
            ctx.project.as_ref().map_or("none", |p| p.kind.as_str()),
            ctx.estimated_tokens
        );
        for constraint in &ctx.constraints {
            if constraint.starts_with("stale CMake build dir ") {
                eprintln!("{}", ui::safe_multiline(constraint));
            }
        }
    }
    let prepared = pipeline::prepare(harness, &raw, request, |feedback, cancel| {
        generator.generate(&raw, request, feedback, cancel)
    });
    let (candidate, last_exit) = match prepared {
        Preparation::Candidate(candidate) => (candidate, None),
        Preparation::Information { title, detail } => {
            println!(
                "{}",
                ui::render_info_at_width(&title, &detail, width, &style)
            );
            return 0;
        }
        Preparation::Cancelled => return 130,
        Preparation::Failed(error) => {
            if !interactive {
                return io_error("inference failed", error);
            }
            (
                pipeline::verify_candidate(
                    harness,
                    &raw,
                    request,
                    String::new(),
                    Some(format!("inference failed: {error}")),
                ),
                Some(2),
            )
        }
    };
    if cli.cd_hook_dir.is_none()
        && gico::intent::classify_str(request) == gico::intent::Intent::NavigateDir
        && candidate.report.verdict() != Verdict::Blocked
    {
        println!("{}", ui::render_info_at_width(
            &candidate.command,
            "Copy this command into your shell, or enable the opt-in Zsh hook with gico init-zsh.",
            width,
            &style,
        ));
        return 0;
    }
    if !interactive {
        println!(
            "{}",
            ui::render_preview(
                &candidate.command,
                &candidate.report,
                candidate.note.as_deref()
            )
        );
        println!("(non-interactive: suggestion only)");
        return 0;
    }
    interact_request(generator, request, harness, raw, candidate, last_exit)
}

fn interact_request(
    generator: &Generator<'_>,
    request: &str,
    harness: &mut Harness,
    mut raw: RawScan,
    mut candidate: Candidate,
    mut last_exit: Option<u8>,
) -> u8 {
    let style = ui::Style::detect();
    let mut raw_guard = match interact::enable_raw_mode() {
        Ok(guard) => guard,
        Err(error) => return io_error("read terminal approval", error),
    };
    let cancel = harness.cancel_token();
    let mut runtime_failures = 0;
    let mut last_failure = None;
    loop {
        if cancel.is_cancelled() {
            harness.record(OpKind::Approve, &candidate.command, OpOutcome::Cancelled);
            return 130;
        }
        let suggestion = ui::render_suggestion(
            &candidate.command,
            &candidate.report,
            interact::term_width(),
            &style,
            candidate.note.as_deref(),
        );
        if interact::write_raw_block(&suggestion).is_err() {
            return 2;
        }
        let action = match interact::read_action() {
            Ok(Some(action)) => action,
            Ok(None) => continue,
            Err(_) if cancel.is_cancelled() => return 130,
            Err(error) => return io_error("read terminal approval", error),
        };
        match action {
            interact::Action::Run => {
                if candidate.report.verdict() == Verdict::Blocked {
                    harness.record(
                        OpKind::Approve,
                        &candidate.command,
                        OpOutcome::Blocked("blocked candidate".into()),
                    );
                    candidate.note =
                        Some("Enter cannot run a blocked command. Use e edit or r refine.".into());
                    continue;
                }
                raw = match scan(generator.engine, &raw.environment.cwd, harness) {
                    Ok(raw) => raw,
                    Err(error) => return io_error("refresh verification evidence", error),
                };
                let approved = match pipeline::authorize(
                    harness,
                    &raw,
                    request,
                    &candidate,
                    Some(&candidate.command),
                ) {
                    Ok(approved) => approved,
                    Err(error) => {
                        candidate.report.issues.push(verify::Issue {
                            severity: verify::Severity::Error,
                            message: error,
                            suggestion: Some(
                                "edit or refine the command to verify it again".into(),
                            ),
                        });
                        continue;
                    }
                };
                drop(raw_guard);
                if approved.is_navigation() && generator.cli.cd_hook_dir.is_none() {
                    println!(
                        "Copy {} into your shell, or enable gico init-zsh.",
                        candidate.command
                    );
                    return 0;
                }
                let timeout =
                    std::time::Duration::from_secs(generator.cli.exec_timeout.unwrap_or(3600));
                let started = std::time::Instant::now();
                let outcome =
                    approved.dispatch(generator.cli.cd_hook_dir.as_deref(), &cancel, timeout);
                let elapsed = started.elapsed().as_millis();
                match outcome {
                    Ok(Dispatch::Navigation) => {
                        harness.record_ms(
                            OpKind::Navigate,
                            "handed off to the parent shell",
                            OpOutcome::Ok,
                            elapsed,
                        );
                        return 0;
                    }
                    Ok(Dispatch::Shell(outcome)) => {
                        let code = outcome.shell_code() as u8;
                        let (operation_outcome, message) = if outcome.timed_out {
                            (
                                OpOutcome::Interrupted("timed out".into()),
                                "timed out (exit 124)".to_string(),
                            )
                        } else if outcome.cancelled || outcome.signal == Some(libc::SIGINT) {
                            let recorded = if outcome.started {
                                OpOutcome::Interrupted("cancelled".into())
                            } else {
                                OpOutcome::Cancelled
                            };
                            harness.record_ms(
                                OpKind::Execute,
                                &candidate.command,
                                recorded,
                                elapsed,
                            );
                            return 130;
                        } else if outcome.success {
                            harness.record_ms(
                                OpKind::Execute,
                                &candidate.command,
                                OpOutcome::Ok,
                                elapsed,
                            );
                            return 0;
                        } else {
                            let message = format!("exited with code {code}");
                            (OpOutcome::Failed(message.clone()), message)
                        };
                        harness.record_ms(
                            OpKind::Execute,
                            &candidate.command,
                            operation_outcome,
                            elapsed,
                        );
                        if outcome.timed_out {
                            eprintln!("gico: command {message}");
                            return code;
                        }
                        last_exit = Some(code);
                        last_failure = Some(format!("{message}. stderr: {}", outcome.stderr_tail));
                        candidate.note = Some(format!(
                            "previous run {message} — r refine with the error, e edit, c cancel"
                        ));
                    }
                    Err(error) => {
                        let kind = if matches!(
                            gico::intent::classify_str(request),
                            gico::intent::Intent::NavigateDir
                        ) {
                            OpKind::Navigate
                        } else {
                            OpKind::Execute
                        };
                        harness.record_ms(
                            kind,
                            &candidate.command,
                            OpOutcome::Failed(error.to_string()),
                            elapsed,
                        );
                        if cancel.is_cancelled() {
                            return 130;
                        }
                        last_exit = Some(2);
                        last_failure = Some(error.to_string());
                        candidate.note = Some(format!("execution failed: {error}"));
                    }
                }
                runtime_failures += 1;
                if runtime_failures >= 3 {
                    eprintln!("gico: command failed after {runtime_failures} approved attempts");
                    return last_exit.unwrap_or(2);
                }
                raw_guard = match interact::enable_raw_mode() {
                    Ok(guard) => guard,
                    Err(_) => return last_exit.unwrap_or(2),
                };
            }
            interact::Action::Refine => {
                let prompt = ui::render_prompt_line("Refine ›", "", &style);
                let Ok(Some(line)) = interact::read_line(&prompt, "") else {
                    continue;
                };
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let feedback = match &last_failure {
                    Some(failure) => format!("{line}. Previous attempt: {failure}"),
                    None => line.to_owned(),
                };
                drop(raw_guard);
                let prepared =
                    pipeline::generate_candidate(harness, &raw, request, Some(line), |cancel| {
                        generator.generate(
                            &raw,
                            request,
                            Some((&candidate.command, &feedback)),
                            cancel,
                        )
                    });
                match prepared {
                    Preparation::Candidate(new) => {
                        if last_failure.is_none() {
                            last_exit = None;
                        }
                        candidate = new;
                    }
                    Preparation::Information { title, .. } | Preparation::Failed(title) => {
                        candidate.note = Some(title)
                    }
                    Preparation::Cancelled => return 130,
                }
                raw_guard = match interact::enable_raw_mode() {
                    Ok(guard) => guard,
                    Err(error) => return io_error("read terminal approval", error),
                };
            }
            interact::Action::Edit => {
                let prompt = ui::render_prompt_line("Edit ›", "", &style);
                if let Ok(Some(line)) = interact::read_line(&prompt, &candidate.command) {
                    let line = line.trim();
                    if !line.is_empty() && line != candidate.command {
                        candidate = pipeline::verify_candidate(
                            harness,
                            &raw,
                            request,
                            line.to_owned(),
                            None,
                        );
                    }
                }
            }
            interact::Action::Cancel => {
                harness.record(OpKind::Approve, &candidate.command, OpOutcome::Cancelled);
                drop(raw_guard);
                return last_exit.unwrap_or(0);
            }
        }
    }
}

fn io_error<T: From<u8>>(operation: &str, error: impl std::fmt::Display) -> T {
    eprintln!(
        "gico: {operation}: {}",
        ui::safe_multiline(&error.to_string())
    );
    2.into()
}
