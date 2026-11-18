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
use gico::config::Config;
use gico::context::{ContextEngine, EngineConfig, RawScan};
use gico::harness::pipeline::{self, Candidate, Dispatch, Preparation};
use gico::harness::{Harness, InterruptGuard, OpKind, OpOutcome};
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

    /// Model name (overrides GICO_MODEL and config file).
    #[arg(long)]
    model: Option<String>,

    /// Provider: ollama, llamacpp (compatible local server), or openai (Responses API).
    #[arg(long)]
    backend: Option<String>,

    /// Native OpenAI reasoning effort (cloud default: none).
    #[arg(long)]
    reasoning_effort: Option<String>,

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

    /// Print the verified decision as JSON without executing it.
    #[arg(long)]
    proposal_json: bool,

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
    /// Show aggregate OpenAI Responses request, token and cost counters.
    Usage,
    /// Manage the OpenAI key in the desktop Secret Service.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
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
    /// Show the effective configuration, installed model and runtime state.
    Status,
    /// Associate a user-chosen name with the current project.
    Alias { name: String },
    /// Save these settings after verifying the model is installed.
    Setup {
        /// Enable approved directory changes and quiet interactive preload.
        #[arg(long, conflicts_with = "remove_zsh")]
        zsh: bool,
        /// Remove GiCo's managed startup entry.
        #[arg(long)]
        remove_zsh: bool,
    },
    /// Warm the configured model using the provider's native idle timer.
    Preload,
    /// Print the opt-in zsh integration snippet (eval "$(gico init-zsh)").
    InitZsh,
}

#[derive(Subcommand)]
enum AuthAction {
    Login,
    Logout,
}

/// Runs the approved directory change in the calling Zsh process.
const ZSH_HOOK_SNIPPET: &str = r#"# GiCo shell integration (opt-in). Add to ~/.zshrc:
#   eval "$(gico init-zsh)"
if [[ -o interactive && -t 0 && -t 1 && -n "$TTY" && "${GICO_TERMINAL:-}" != "$TTY" ]]; then
  export GICO_TERMINAL="$TTY"
  command gico preload </dev/null >/dev/null 2>&1 &!
fi
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
    // Allow shell startup and version discovery even if the config needs repair.
    if matches!(cli.command, Some(Cmd::InitZsh)) {
        let cloud = gico::config::path()
            .ok()
            .and_then(|p| Config::load(&p).ok())
            .is_some_and(|c| Backend::parse(&c.backend) == Some(Backend::OpenAiResponses));
        if cloud {
            print!(
                "{}",
                ZSH_HOOK_SNIPPET
                    .replace("  command gico preload </dev/null >/dev/null 2>&1 &!\n", "")
            );
        } else {
            print!("{ZSH_HOOK_SNIPPET}");
        }
        return ExitCode::SUCCESS;
    }
    if matches!(
        cli.command,
        Some(Cmd::Setup {
            remove_zsh: true,
            ..
        })
    ) {
        return match gico::shell::configure(false) {
            Ok(p) => {
                println!("Updated {}", p.display());
                ExitCode::SUCCESS
            }
            Err(e) => io_error("Zsh configuration", e),
        };
    }
    if let Some(Cmd::Auth { ref action }) = cli.command {
        return match match action {
            AuthAction::Login => gico::auth::login(),
            AuthAction::Logout => gico::auth::logout(),
        } {
            Ok(()) => {
                println!("GiCo OpenAI keyring entry updated.");
                ExitCode::SUCCESS
            }
            Err(e) => io_error("authentication", e),
        };
    }
    let path = match cli
        .config
        .clone()
        .map(Ok)
        .unwrap_or_else(gico::config::path)
    {
        Ok(path) => path,
        Err(e) => return io_error("config location", e),
    };
    let mut cfg = match Config::load(&path) {
        Ok(cfg) => cfg,
        Err(e) => return io_error("configuration", e),
    };
    if let Some(value) = &cli.backend {
        cfg.select_backend(value);
    }
    if let Some(value) = &cli.reasoning_effort {
        cfg.reasoning_effort.clone_from(value);
    }
    if let Some(value) = &cli.model {
        cfg.model.clone_from(value);
    }
    if let Some(value) = &cli.endpoint {
        cfg.endpoint.clone_from(value);
    }
    if let Some(value) = cli.budget {
        cfg.budget_tokens = value;
    }
    if let Some(value) = cli.temperature {
        cfg.temperature = value;
    }
    if let Some(value) = cli.max_tokens {
        cfg.max_tokens = value;
    }
    let mut client = match cfg.client() {
        Ok(c) => c,
        Err(e) => return io_error("configuration", e),
    };
    client.debug = cli.debug;
    let budget = cfg.budget_tokens;
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
        Some(Cmd::Usage) => match gico::usage::read() {
            Ok(t) => {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&t).expect("usage counters")
                );
                ExitCode::SUCCESS
            }
            Err(e) => io_error("usage totals", e),
        },
        Some(Cmd::Auth { .. }) => unreachable!("handled before loading configuration"),
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
        Some(Cmd::Alias { name }) => {
            let result = std::env::current_dir()
                .and_then(|p| engine.scan(&p))
                .and_then(|raw| {
                    gico::context::registry::observe(&raw)?;
                    gico::context::registry::alias(&raw.environment.cwd, &name)
                });
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => io_error("save alias", e),
            }
        }
        Some(Cmd::Preload) => match gico::lifecycle::warm(&cfg) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => io_error("preload", e),
        },
        Some(Cmd::Setup { .. } | Cmd::Status) => {
            let setup = matches!(cli.command, Some(Cmd::Setup { .. }));
            println!(
                "config: {} ({})",
                path.display(),
                if path.exists() {
                    "present"
                } else {
                    "using defaults"
                }
            );
            println!(
                "backend: {} at {}\nmodel: {}",
                cfg.backend, client.endpoint, cfg.model
            );
            if client.backend == Backend::OpenAiResponses {
                println!(
                    "Responses API · reasoning {} · output limit {} · local preload disabled",
                    cfg.reasoning_effort, cfg.max_tokens
                );
            } else {
                println!(
                    "context: {} tokens; output: {}; temperature: {}\npreload: {}; idle: {}s",
                    cfg.context_size,
                    cfg.max_tokens,
                    cfg.temperature,
                    cfg.preload,
                    cfg.idle_seconds
                );
            }
            match client.list_models() {
                Ok(models) if models.contains(&cfg.model) => (),
                Ok(_) => {
                    return io_error(
                        "model missing",
                        format!(
                            "install {} on {} or select an installed model with --model",
                            cfg.model, cfg.endpoint
                        ),
                    )
                }
                Err(e) => return io_error("provider status", e),
            }
            if client.backend == Backend::Ollama {
                let details = match client.details() {
                    Ok(data) => data,
                    Err(e) => return io_error("model metadata", e),
                };
                println!(
                    "Ollama {} · {} · digest {}",
                    details["version"].as_str().unwrap_or("unknown"),
                    details["quantization"].as_str().unwrap_or("unknown"),
                    details["digest"].as_str().unwrap_or("unknown")
                );
                if details["capabilities"]
                    .as_array()
                    .is_some_and(|a| !a.iter().any(|v| v == "completion"))
                {
                    return io_error(
                        "unsupported model",
                        "select a local model with completion capability",
                    );
                }
            }
            if setup {
                if matches!(cli.command, Some(Cmd::Setup { zsh: true, .. })) {
                    if let Err(e) = gico::shell::configure(true) {
                        return io_error("Zsh configuration", e);
                    }
                }
                if let Err(e) = cfg.save(&path) {
                    return io_error("save settings", e);
                }
            }
            match client.backend {
                Backend::Ollama => match client.resident_context() {
                    Ok(Some(n)) if n == cfg.context_size as u64 => println!("runner: resident"),
                    Ok(_) if gico::lifecycle::loading(&cfg) => println!("runner: loading"),
                    Ok(Some(n)) => println!(
                        "runner: resident with {n} context tokens (configured {})",
                        cfg.context_size
                    ),
                    Ok(None) => println!("runner: installed, unloaded"),
                    Err(e) => return io_error("runner status", e),
                },
                Backend::OpenAiResponses => println!("model access: available"),
                Backend::OpenAiCompatible => {
                    println!("runner: provider-managed; native preload unavailable")
                }
            }
            ExitCode::SUCCESS
        }
        Some(Cmd::Models) => {
            let backend_s = &cfg.backend;
            let endpoint = &cfg.endpoint;
            let Some(_backend) = Backend::parse(backend_s) else {
                eprintln!("gico: unknown backend '{backend_s}' (use ollama or llamacpp)");
                return ExitCode::from(2);
            };

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
            run_request(&engine, &request, &cli, client)
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
    client: LlmClient,
    loading_checked: std::cell::Cell<bool>,
}

impl Generator<'_> {
    fn generate(
        &self,
        raw: &RawScan,
        request: &str,
        feedback: Option<(&str, &str)>,
        harness: &mut Harness,
    ) -> Result<gico::discovery::Reply, String> {
        if let Ok(mock) = std::env::var("GICO_MOCK_COMMAND") {
            return Ok(gico::discovery::Reply::Command(mock));
        }
        let cold = std::io::stderr().is_terminal()
            && std::env::var_os("GICO_TERMINAL").is_some()
            && !self.loading_checked.replace(true)
            && self.client.backend == Backend::Ollama
            && self.client.resident().is_ok_and(|v| !v);
        let _loading = cold
            .then(|| {
                gico::lifecycle::loading_marker(
                    &self.client.endpoint,
                    &self.client.model,
                    self.client.context_size,
                )
            })
            .flatten();
        if cold && std::io::stderr().is_terminal() && std::env::var_os("GICO_TERMINAL").is_some() {
            eprintln!(
                "  gico › loading {}",
                ui::safe_multiline(&self.client.model)
            );
        }
        gico::discovery::interpret(
            &self.client,
            self.engine,
            raw,
            request,
            feedback,
            self.cli.no_context,
            harness,
        )
    }
}

fn run_request(engine: &ContextEngine, request: &str, cli: &Cli, client: LlmClient) -> ExitCode {
    let _interrupts = match InterruptGuard::install() {
        Ok(guard) => guard,
        Err(error) => return io_error("install cancellation handler", error),
    };
    let generator = Generator {
        engine,
        cli,
        client,
        loading_checked: Default::default(),
    };
    let mut harness = Harness::new(5);
    harness.enable_interrupts();
    let code = run_pipeline(&generator, request, &mut harness);
    if code == 0
        && generator.client.backend == Backend::Ollama
        && cli.cd_hook_dir.is_some()
        && std::io::stdin().is_terminal()
        && std::env::var_os("GICO_TERMINAL").is_some()
        && !harness
            .ops()
            .iter()
            .any(|op| matches!(op.outcome, OpOutcome::Cancelled))
    {
        if let Ok(exe) = std::env::current_exe() {
            let mut warm = std::process::Command::new(exe);
            if let Some(path) = &cli.config {
                warm.arg("--config").arg(path);
            }
            warm.args([
                "--model",
                &generator.client.model,
                "--backend",
                match generator.client.backend {
                    Backend::Ollama => "ollama",
                    Backend::OpenAiCompatible => "llamacpp",
                    Backend::OpenAiResponses => "openai",
                },
                "--endpoint",
                &generator.client.endpoint,
                "preload",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
            let _ = warm.spawn();
        }
    }
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
    let selected = match gico::context::registry::project_for(&cwd, request) {
        Ok(p) => p,
        Err(question) => {
            if cli.proposal_json {
                println!("{}", serde_json::json!({"kind":"clarify","text":question}));
                return 0;
            }
            if !interactive {
                println!("{}", ui::render_info_at_width(&question, "", width, &style));
                return 0;
            }
            let guard = match interact::enable_raw_mode() {
                Ok(g) => g,
                Err(e) => return io_error("clarification", e),
            };
            let prompt =
                ui::render_prompt_line(&format!("{} ›", ui::safe_multiline(&question)), "", &style);
            let answer = interact::read_line(&prompt, "");
            drop(guard);
            let Ok(Some(answer)) = answer else { return 130 };
            let answer = answer.trim();
            let explicit = PathBuf::from(answer);
            if explicit.is_absolute() && explicit.is_dir() {
                Some(explicit)
            } else {
                match gico::context::registry::project_for(&cwd, &format!("go to {answer}")) {
                    Ok(Some(path)) => Some(path),
                    _ => {
                        return io_error(
                            "project location",
                            "could not resolve that choice; specify an existing project path",
                        )
                    }
                }
            }
        }
    };
    let nav = gico::intent::navigation_destination(request).is_some();
    let workdir = if nav {
        &cwd
    } else {
        selected.as_ref().unwrap_or(&cwd)
    };
    let raw = match scan(generator.engine, workdir, harness) {
        Ok(raw) => raw,
        Err(error) => return io_error("inspect working directory", error),
    };
    if !cli.no_cache {
        if let Err(e) = gico::context::registry::observe(&raw) {
            eprintln!("gico: project context unavailable: {e}");
        }
    }
    if cli.json || cli.dry_run || cli.verbose || cli.debug {
        let ctx = generator.engine.context_from_scan(&raw, request);
        if cli.json {
            println!("{}", serde_json::to_string_pretty(&ctx).unwrap());
            return 0;
        }
        if cli.dry_run {
            print_context(&ctx);
            let prompt = gico::discovery::prompt(
                &generator.client,
                generator.engine,
                &raw,
                request,
                None,
                cli.no_context,
            )
            .0;
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
    if workdir != &cwd && !cli.proposal_json {
        println!(
            "{}",
            ui::render_info_at_width(
                "Working directory",
                &workdir.display().to_string(),
                width,
                &style
            )
        );
    }
    let mut prepared = if let Some(target) = selected.as_ref().filter(|_| nav) {
        Preparation::Candidate(pipeline::verify_candidate(
            harness,
            &raw,
            request,
            format!(
                "cd {}",
                gico::context::fs::shell_quote(&target.to_string_lossy())
            ),
            None,
        ))
    } else {
        pipeline::prepare(harness, &raw, request, |feedback, cancel| {
            generator.generate(&raw, request, feedback, cancel)
        })
    };
    if cli.proposal_json {
        let obj = match &prepared {
            Preparation::Candidate(c) => {
                serde_json::json!({"kind":"proposal","cwd":workdir,"command":c.command,"argv":c.argv(),"verdict":c.report.verdict(),"issues":c.report.issues,"note":c.note})
            }
            Preparation::Information { title, detail } => {
                serde_json::json!({"kind":"answer","text":format!("{title} {detail}")})
            }
            Preparation::Clarify(q) => serde_json::json!({"kind":"clarify","text":q}),
            Preparation::Failed(e) => serde_json::json!({"kind":"failed","text":e}),
            Preparation::Cancelled => return 130,
        };
        println!("{obj}");
        return if matches!(prepared, Preparation::Failed(_)) {
            2
        } else {
            0
        };
    }
    let mut clarified = request.to_owned();
    if let Preparation::Clarify(question) = &prepared {
        if !interactive {
            println!("{}", ui::render_info_at_width(question, "", width, &style));
            return 0;
        }
        let guard = match interact::enable_raw_mode() {
            Ok(v) => v,
            Err(e) => return io_error("clarification", e),
        };
        let question =
            ui::render_prompt_line(&format!("{} ›", ui::safe_multiline(question)), "", &style);
        let answer = interact::read_line(&question, "");
        drop(guard);
        let Ok(Some(answer)) = answer else {
            return 130;
        };
        clarified.push_str("\nClarification: ");
        clarified.push_str(&answer);
        prepared = pipeline::prepare(harness, &raw, &clarified, |feedback, h| {
            generator.generate(&raw, &clarified, feedback, h)
        });
    }
    let request = clarified.as_str();
    let (candidate, last_exit) = match prepared {
        Preparation::Candidate(candidate) => (candidate, None),
        Preparation::Information { title, detail } => {
            println!(
                "{}",
                ui::render_info_at_width(&title, &detail, width, &style)
            );
            return 0;
        }
        Preparation::Clarify(question) => {
            println!("{}", ui::render_info_at_width(&question, "", width, &style));
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
    let selected = selected.unwrap_or_else(|| raw.environment.cwd.clone());
    let code = interact_request(generator, request, harness, raw, candidate, last_exit);
    if code == 0
        && harness
            .ops()
            .iter()
            .any(|op| matches!(op.kind, OpKind::Execute) && matches!(op.outcome, OpOutcome::Ok))
    {
        gico::context::registry::remember(&selected);
    }
    code
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
                        if let Some(target) =
                            verify::navigation_target(&candidate.command, &raw.environment.cwd)
                        {
                            gico::context::registry::remember(&target);
                        }
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
                harness.begin_refinement();
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
                    Preparation::Information { title, .. }
                    | Preparation::Clarify(title)
                    | Preparation::Failed(title) => candidate.note = Some(title),
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
