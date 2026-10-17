# Necropolis — Architecture & Security Decisions Log

## ADR-001: Pure Rust Core Workspace
- **Decision:** All components (`necro` CLI, scanner, redactor, capability sandbox, git wrapper, Tree-sitter AST parser, summarizer, Postgres store, and MCP stdio server) are implemented in a single Rust 2024 workspace with `#![forbid(unsafe_code)]`.
- **Rationale:** Eliminates cross-language schema drift, dual secret-redaction stacks, and memory-safety risks when parsing untrusted files across hundreds of local repositories.

## ADR-002: Single-Stage In-Process Secret Redaction (`necro-redact`)
- **Decision:** Run linear-time (`regex` DFA + `aho-corasick` keyword prefilter + Shannon entropy check) secret redaction at three boundaries:
  1. Pre-index (before AST symbol extraction or DB persistence)
  2. Pre-LLM (before any prompt leaves the machine)
  3. Egress (before any MCP tool response or `get_pointer` slice is returned)
- **Rationale:** Defense-in-depth against accidental secret indexing and exfiltration via MCP tool calls.

## ADR-003: Capability-Based Filesystem Sandbox (`necro-sandbox`) & Hardened Git Wrapper (`necro-git`)
- **Decision:** Use `cap-std::fs::Dir` (`openat2(RESOLVE_BENEATH)` on Linux) anchored at registered root directories, combined with a `globset` path denylist and an indexed-files-only DB check. Git historical blob reads validate `^[0-9a-f]{40|64}$`, verify `git cat-file -t <sha> == commit`, and read via `git cat-file blob <sha>:<rel_path>` with locked environment variables.
- **Rationale:** Prevents symlink escapes, TOCTOU races, path traversal (`../`), and git flag/argument injection (`--output=...`).

## ADR-004: Stored Prompt-Injection Defenses & Provenance Envelope
- **Decision:** L0 cards are validated against `schemas/card.schema.json` and scanned by a deterministic instruction/shell-command linter (`necro-summarize`). Shell commands are isolated in `commands: Vec<String>`. Suspicious cards are quarantined (`status = 'quarantined'`) until reviewed via `necro review`. All MCP responses wrap results in `{ "untrusted_derived_data": true, "provenance": "derived_from_local_repo_text", "data": ... }`.
- **Rationale:** Prevents hostile READMEs or comments in third-party repos from hijacking an LLM agent consuming Necropolis over MCP.
