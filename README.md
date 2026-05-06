# hhagent

A personal, always-on agentic system designed from the ground up for security and vendor neutrality.

## What it is

A long-running personal AI agent that:

- talks to you over secure messaging (Telegram, Signal) and email (its own IMAP/SMTP account)
- remote-controls a web browser, performs web searches and page fetches
- executes Python in a strict sandbox
- maintains persistent memory in Postgres with hybrid retrieval (pgvector + BM25 + graph)
- runs continuously, periodically resetting its context window from memories and a persistent task list

## Design priorities (in order)

1. **Security boundary = the agent's own OS user account.** Worst-case compromise (LLM, tool, dependency, or LLM-authored Python) does not escape that boundary.
2. **Vendor neutrality.** Primary host is the NVIDIA DGX Spark, but no hard NVIDIA dependency. Linux and macOS are both first-class.
3. **License hygiene.** Project is AGPL-3.0; every dependency is AGPL-compatible.
4. **Small core.** The agent core is Rust (no eval, no metaprogramming, no dynamic import). Python lives only inside sandboxed workers.

## Status

Early scaffold. See [`docs/architecture.md`](docs/architecture.md) and [`docs/threat-model.md`](docs/threat-model.md). Phased build plan is tracked in the design plan file outside this repo.

## Layout

```
core/          Rust agent core (scheduler, memory, policy, LLM router, audit, IPC)
sandbox/       Cross-platform sandbox crate (bwrap+Landlock on Linux, Seatbelt on macOS)
supervisor/    Service-supervisor abstraction (systemd --user / launchd LaunchAgents)
workers/       Tool workers, each its own sandboxed process
adapters/      Channel adapters (Telegram, Signal)
db/migrations/ Postgres schema (pgvector, pg_search, Apache AGE)
config/        Runtime policy and per-worker sandbox profiles
docs/          Architecture & threat-model docs
```

## License

AGPL-3.0-only. See [LICENSE](LICENSE).
