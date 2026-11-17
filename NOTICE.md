# GiCo upstream notices

GiCo incorporates source from OpenAI Codex and a Rust adaptation of parts of
DeepSeek Harness. The projects are independent.

## OpenAI Codex

[Source at bc2c328483ee60e82e08ad48f3a5efbb5712f914][codex], inspected
2026-11-17. Apache-2.0. The distribution includes the [license](licenses/Apache-2.0-Codex.txt)
and [upstream NOTICE](licenses/Codex-NOTICE.txt).

- `codex-rs/utils/head-tail-buffer/src/lib.rs` is vendored verbatim in
  `src/vendor/codex_head_tail_buffer.rs`. It buffers raw output bytes with
  a fixed prefix and suffix and reports omitted bytes. It requires only
  the Rust standard library. Its upstream tests use the standard
  `assert_eq!` macro in GiCo.
- `codex-rs/utils/pty/src/process_group.rs` supplies the Unix
  `set_process_group` and `signal_process_group_id` functions. GiCo
  retains those functions in `src/vendor/codex_process_group.rs`, adds a
  local import, drops the platform guards for its Unix implementation,
  and changes the signal helper’s visibility. The extract needs only
  `libc`. Terminal foreground handoff and timeout handling live in GiCo.
- GiCo retains the same third-party `shlex` 1.3 series used by
  `codex-rs/shell-command`. The verifier's wrapper-depth limit also draws
  on `shell-command/src/command_safety/is_dangerous_command.rs`.

Reviewed and omitted `core/src/session/turn.rs`, `core/src/tools/registry.rs`,
`core/src/tools/orchestrator.rs`, `core/src/exec.rs`, the full shell and PTY
crates, context compaction, sandboxing, and the Codex provider and terminal
interfaces. Their protocol, tree-sitter, async PTY, workspace, or provider
requirements exceed what GiCo needs for one explicitly approved command.

## DeepSeek Harness

[Source at ca33fd6843b205e279c7ccb6cb73a472c78f5760][deepseek], inspected
2026-11-17. MIT. The [upstream license and copyright](licenses/DeepSeek-MIT.txt)
are included.

- `packages/core/tools/src/pipeline.ts` and `contracts.ts` supply the
  prepare, authorize, dispatch and settle sequence. The Rust adaptation uses
  GiCo's verified candidate, structured results and consumed approval value.
- `packages/interaction/user-approval/src/index.ts` supplies the one-use grant
  and fail-closed decision semantics. GiCo binds the decision to the
  displayed command and working directory.
- `packages/core/agent-loop/src/agent.ts` and
  `packages/core/session/src/repair.ts` inform cancellation and settlement.
  GiCo records whether execution started and requires fresh approval for
  another dispatch. The inference budget is GiCo's own.

Reviewed and omitted the TypeScript agent loop, model tool registration,
scoped Cordis plugins, session persistence, prompt assembly and compaction,
and the Bash and sandbox providers. These rely on Node.js and workspace
packages or solve multi-step session problems outside GiCo's workflow.
The Rust adaptation imports no TypeScript runtime or package.

## Rust dependencies

The dependency audit used Cargo's locked metadata for
`x86_64-unknown-linux-gnu`. `shlex` 1.3.0 is MIT OR Apache-2.0.
`libc` 0.2.191 is MIT OR Apache-2.0 and already supplied Unix process calls.
Tokio 1.54.1 was already transitive through `reqwest`. GiCo now uses it
directly with `rt`, `time`, and `macros` for cancellable HTTP requests.
Tokio and the newly enabled `tokio-macros` 2.7.2 are MIT. The retained
`reqwest` 0.12.28 dependency is MIT OR Apache-2.0. No Codex workspace
crate or DeepSeek package is in the Cargo dependency graph.

[codex]: https://github.com/openai/codex/tree/bc2c328483ee60e82e08ad48f3a5efbb5712f914
[deepseek]: https://github.com/deepseek-ai/deepseek-harness/tree/ca33fd6843b205e279c7ccb6cb73a472c78f5760
