# GiCo

GiCo is a Rust terminal assistant for Linux. It turns a request into a shell
command using evidence from the current directory and, when needed, a small
local model. You can inspect, refine, or edit the command before running it.

```text
$ gico "list files in this directory"
  gico › ls -la
         ↵ run · r refine · e edit · c cancel
```

## Get started

Install with the stable Rust toolchain and Cargo.

```sh
cargo install --locked --git https://github.com/golba98/GiCo.git --tag v0.1.2 --bin gico
gico --version
```

For requests that need a model, start [Ollama](https://ollama.com/) and download
the model used in GiCo's benchmarks.

```sh
ollama pull qwen2.5-coder:0.5b
export GICO_MODEL=qwen2.5-coder:0.5b
gico "print a short greeting with printf"
```

Ollama defaults to http://127.0.0.1:11434. For a running llama.cpp server or
another OpenAI-compatible endpoint, use:

```sh
gico --backend llamacpp --endpoint http://127.0.0.1:8080 models
gico --backend llamacpp --endpoint http://127.0.0.1:8080 --model MODEL "print a short greeting with printf"
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
```

Directory changes can be applied in the calling Zsh session with the opt-in
hook. Load it once in the current shell or add the line to `~/.zshrc`:

```sh
eval "$(gico init-zsh)"
gico "change to my Rust development directory"
```

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
user's permissions, so review them before pressing Enter. GUI detection
uses available CMake evidence and requires a display server to launch.

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

The legacy suite has 107 cases. In one matched Fedora 45 run, the original
implementation passed 60 and GiCo 0.1.1 passed 61. GiCo 0.1.1 passed 11 of
12 additional cases in the holdout suite. Use `--suite holdout` for those
cases and pass `--exec` in an isolated fixture environment to measure actual
command execution.

## License

GiCo's original code is available under the [MIT License](LICENSE).
The distribution also contains Apache-2.0 code from OpenAI Codex and an
adaptation of MIT-licensed DeepSeek Harness code. See [NOTICE.md](NOTICE.md)
and [licenses/](licenses/) for attribution and the upstream terms.
