# The C and C++ port: plan and status

The owner asked for the codebase rewritten in C and C++ "for maximum speed", on the branch
`claude/c-cpp-rewrite-and-site`, with a SvelteKit site for download, landing and docs. This file
is the working record; the pull request's description is the status page.

## Split (a decision for the owner)

- **Engine in C** (`core/`, today `crates/core`): pure, as SPEC §4.2 requires: no I/O, no clock,
  no threads, no global randomness; the caller passes time and random bytes. C keeps the purity
  rule checkable by what the library links, and gives netsim and the daemon one ABI.
- **Everything else in C++** (daemon, OS layer, CLI, updater, simulator, load tool, tests): the
  daemon's JSON, strings, containers and per-OS code are where C++'s standard library saves the
  most lines.

## Gates, in order

1. Same behaviour, proven by tests ported along with the code. The Rust tree is the oracle and
   stays in place and building until parity is shown.
2. Wire and CLI compatibility: protocol v2, config v2, the status JSON, every command of
   `wirebond help`; the system harness with a C++ client against a Rust anchor, and the reverse.
3. Measured speed: the engine benchmark and the harness's A12, Rust against C/C++ on one machine.
4. Security at least as good: a vetted crypto library, sanitizers, fuzzed parsers, zeroed keys.
5. The standards: few vendored dependencies, one static executable per platform no larger than
   today's, a formatter and warnings as errors in CI, the same eight release assets.

## Baseline on this machine (Rust 1.99.1)

- `cargo test -p wirebond-netsim --release`: 10 of 11 acceptance scenarios pass, `a4` ignored.
- `cargo test -p wirebond`: 122 of 123 pass; `host::tests::on_linux::missing_tools_are_named`
  fails in this container (to be explained before it is used as a gate).
