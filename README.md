# GiCo

GiCo is a Rust terminal assistant for Linux. It turns a request into a shell
command using evidence from the current directory and, when needed, a small
local model or an explicitly selected OpenAI provider. You can inspect, refine, or edit the command before running it.

```text
$ gico "list files in this directory"
  gico › ls -la
         ↵ run · r refine · e edit · c cancel
```

## Get started

Build from the checkout with the stable Rust toolchain. Put Cargo's bin directory
on your shell's PATH.

```sh
cargo install --locked --path . --bin gico --bin gico-bench
gico --version
gico models
```

Select an installed Ollama model once. The following example uses the small
baseline model. If it is missing, install it with `ollama pull qwen2.5-coder:0.5b`.

```sh
gico --model qwen2.5-coder:0.5b setup --zsh
gico status
```

Open a new interactive Zsh terminal. GiCo warms the model quietly in the
background and Ollama releases it after five idle minutes. The shell hook also
applies approved directory changes to the calling shell. To remove the managed
startup entry, run `gico setup --remove-zsh`. To keep directory navigation and
disable startup warming, set `preload = false` in the configuration.

Ollama defaults to `http://127.0.0.1:11434`. For a running llama.cpp server
with structured output support, use its exact reported model identifier.

```sh
gico --backend llamacpp --endpoint http://127.0.0.1:8080 models
gico --backend llamacpp --endpoint http://127.0.0.1:8080 --model MODEL setup
```

## Use GiCo

| Key | Action |
| --- | --- |
| Enter | Run the displayed command if verification allows it |
| r | Refine the suggestion with another instruction |
| e | Edit the command inline and verify it again |
| c, Escape, Ctrl+C | Cancel |

When output is redirected, GiCo prints the suggestion and skips execution.
It supports `NO_COLOR` and wraps commands and notes to fit the terminal.

```sh
gico "compile and run this C++ project"
gico "find the graphical application built by this project and launch it"
gico "what is the default profile described in README.md?"
gico context --json
gico verify "cargo test"
gico --dry-run "build the project"
```

Use `--verbose` for operation details or `--debug` for the inference prompt and
response. CLI options override environment variables such as `GICO_MODEL`,
`GICO_BACKEND`, `GICO_ENDPOINT`, and `GICO_BUDGET`, which override the corresponding
keys in `~/.config/gico/config.toml`.

```toml
model = "qwen2.5-coder:0.5b"
backend = "ollama"
endpoint = "http://127.0.0.1:11434"
budget_tokens = 700
context_size = 4096
max_tokens = 256
temperature = 0.1
preload = true
idle_seconds = 300
```

From a project root, save an alias with `gico alias "pixel tools"`. You can
then refer to that project from another directory. GiCo keeps generated
observations in `$XDG_DATA_HOME/gico` (normally `~/.local/share/gico`).
Each project's `context.json` records the metadata behind its targets and
recipes. Its separate `preferences.json` holds your aliases, purpose and file
roles. Refreshing observations preserves those preferences.

For a question that names a single discovered project document, source file or
manifest, GiCo can read a short excerpt before answering. It withholds excerpts
that appear to contain credentials and asks for the relevant passage instead.

## How it works

- A bounded, read-only scan collects project and environment evidence.
  Detectors recognize common C/C++, Rust, Python, JavaScript/TypeScript, and Go
  projects.
- A deterministic planner handles recognized tasks. Other requests use a
  compact prompt and a bounded inference loop.
- The verifier checks commands against the filesystem and flags unsafe
  patterns. An interactive approval applies to the displayed command and
  working directory. Approved commands support cancellation and timeouts.

Shell verification is heuristic. Approved commands run with the current
user's permissions, so review them before pressing Enter. GiCo resolves supported launches from project manifests, source entrypoints,
and documented recipes. It asks for a choice when there are several matching
targets. Desktop launches require a display server. After a failed command,
GiCo can refresh its evidence and present a correction for a new approval.

For example, try `gico "show me the GUI in this Rust build"` from a Cargo
workspace. With the Zsh hook, `gico "go back where I was"` uses the shell's
previous directory, including manual directory changes. Add absolute paths to
`workspace_roots` in the GiCo configuration to include other workspaces.

## Development

```sh
git clone https://github.com/golba98/GiCo.git
cd GiCo
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

The tests cover command verification, project detection, and real terminal
interaction through PTYs. The test suite can run without a model server.

To run the local-model benchmark:

```sh
cargo run --locked --release --bin gico-bench -- \
  --model qwen2.5-coder:0.5b --suite legacy
```

The legacy suite has 107 cases and the published holdout suite has 12.
Use `--suite all --modes full --exec` for both. Some legacy cases still use
command-pattern checks. For behavioral evaluations, supply a fixture manifest
and a new output filename. These evaluations require Linux user namespaces,
Bubblewrap, Python 3 and pexpect.

```sh
gico-bench --model qwen2.5-coder:0.5b --manifest cases.json --output results.jsonl
```

The behavioral evaluator records the live proposal and inference trace, then
replays the proposal through an isolated approval PTY and checks fixture output
or state. It creates an independent fixture for each case.

## License

GiCo's original code is available under the [MIT License](LICENSE).
The distribution also contains Apache-2.0 code from OpenAI Codex and an
adaptation of MIT-licensed DeepSeek Harness code. See [NOTICE.md](NOTICE.md)
and [licenses/](licenses/) for attribution and the upstream terms.

### Native OpenAI Responses

GiCo can use a cloud model for requests that need inference. Deterministic requests stay local. When cloud inference is used, the request and the selected project evidence go to the configured OpenAI model. GiCo still verifies the proposal and waits for Enter before executing it.

In an interactive terminal, save a project API key in the desktop Secret Service and select the cloud provider:

```sh
gico auth login
gico --backend openai setup
gico status
gico usage
```

Login accepts an existing `OPENAI_API_KEY` or prompts with terminal echo disabled. Linux keyring storage requires `secret-tool` and an unlocked Secret Service. An environment key takes precedence. Use `gico auth logout` to remove the stored entry.

The native backend uses `https://api.openai.com/v1/responses`, with `gpt-6-luna` and reasoning effort `none` by default. Set `--reasoning-effort low` for an individual request or persist it with `setup`. Select a stronger model explicitly with `gico --backend openai --model gpt-6.1-terra --reasoning-effort low "your request"`. Model access depends on the API project. Cloud usage has no local model preload. Usage counters report provider-returned tokens and an estimate at standard short-context list prices.

Use `gico --backend ollama "your request"` for local inference or `gico --backend ollama setup` to restore the saved local selection. A provider error is reported to the terminal. GiCo only switches providers when selected.
