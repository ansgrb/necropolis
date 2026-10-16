# Project Memory Bank — Implementation Blueprint

Name: **Necropolis** (official)
Owner: Anas Ghareib
Status: Planning → Phase 0 (blueprint ready to scaffold)
Last updated: 2026-10-16 (Revision 3; see §16. Precedence: §16 > §15 > earlier sections)

---

## 1. Recap

Every developer has a graveyard of old projects, local and online. They get forgotten, and their ideas, methods and solved problems are never reused. Two ideas came out of this:

1. **Knowledge bank.** Turn every project (finished or abandoned) into a compact, AI-readable map. Agents can query the bank instead of re-reading repositories, which saves tokens and lets old work feed new projects.
2. **Reverse and rebuild.** When only a release artifact (APK, IPA, binary, bundle) survives and the source is gone, automate as much recovery as possible: decompile, map, and generate a rewrite plan and skeleton.

## 2. Digest (the design in one page)

- **Tiered context.** Agents never read more than they need.
  - **L0 card:** about 300–600 tokens per project. This is what an agent reads first.
  - **L1 map:** modules, symbols, dependency graph and ideas, queried on demand.
  - **L2 pointers:** exact file paths and line ranges, so the agent opens only what it needs.
- **Deterministic first, LLM second.** Tree-sitter, manifests and git history do the heavy lifting for free. An LLM writes only the final summary, once per commit hash.
- **Multiple retrieval modes for different question types.**

  | Question type | Example | Engine |
  |---|---|---|
  | Exact lookup | `ERR_CERT_PINNING`, `import dio`, `lib/auth/token.dart` | Tantivy (+ Postgres trigram as fallback) |
  | Semantic / "have I done this before?" | "offline sync with conflict resolution" | pgvector |
  | Structured / relational | "all projects using Riverpod and abandoned after 2023" | Postgres |
  | Analytics | "language mix over time", "most reused modules" | DuckDB over Postgres/Parquet |

- **Delivery through MCP.** One MCP server exposes the bank to Claude Code and any other agent.
- **Reverse engineering feeds the same pipeline.** Decompiler output is normalized into a "synthetic repo" and mapped by the same scanner, so recovery and the knowledge bank share one code path.

---

## 3. Goals and Non-Goals

**Goals**
- A scan of any repo yields an L0 card plus L1 map in one command.
- An agent can answer "have I built X before, and where?" in under 2 seconds and under 1,500 tokens.
- Re-scans are incremental and cost nothing when nothing changed.
- Recover structure, names and logic from release artifacts of your own apps, and emit a rewrite plan plus skeleton.

**Non-goals (for now)**
- A hosted multi-user SaaS.
- Perfect decompilation. The goal is a good map and a plan, not byte-identical source.
- Replacing IDE indexers.
- Reversing software you don't own or have no right to analyze.

---

## 4. Database Decisions

### 4.1 Recommended stack

| Role | Choice | Why |
|---|---|---|
| **System of record** | **PostgreSQL 16+** | Relational integrity, JSONB for cards, transactions, mature tooling |
| **Semantic search** | **pgvector** (HNSW index) | Vectors live next to the metadata, so filtered semantic queries are one SQL statement |
| **Fuzzy text / paths** | `pg_trgm` + built-in FTS | Good enough in Phases 1–3 with zero extra services |
| **Exact identifier search** | **Tantivy** (Rust, embedded) | Fast exact/prefix/regex search over identifiers, imports, error strings and file paths using custom tokenizers |
| **Analytics** | **DuckDB** | Query Parquet exports or attach Postgres for ad-hoc analysis, with no load on the main DB |
| **Embedded/portable vectors** | **LanceDB** (optional) | Local-first mode with no Postgres: columnar, versioned, good for snapshots shipped with a project |

Postgres is the source of truth. Tantivy and LanceDB are **derived indexes** that can be rebuilt from Postgres at any time. Never write to them first.

### 4.2 Evaluated, and when to use each

| Tool | Verdict |
|---|---|
| Pinecone | Skip. Hosted-only, and you don't need a separate vector service at personal scale. |
| Milvus | Skip until you have tens of millions of vectors. Operationally heavy. |
| Qdrant | **Best upgrade path** if pgvector becomes a bottleneck. Great filtering, simple to run. Keep the vector-store interface abstract so swapping is cheap. |
| Chroma | Prototyping only. Skip for the real system. |
| Weaviate | Skip. Overlaps with Postgres + pgvector, with more moving parts. |
| LanceDB | **Optional** embedded mode (see above). |
| DuckDB | **Use** for analytics and Parquet export. Not a primary store. |
| Quickwit | Skip. Built for distributed log analytics; Tantivy embedded already covers your scale. |
| Meilisearch / Typesense | Optional later for a typo-tolerant search UI. Not needed for agent retrieval. |

**Rule:** start with Postgres + pgvector + trigram/FTS. Add Tantivy in Phase 4 when exact identifier lookups prove slow or noisy. Add anything else only when a measured problem justifies it.

### 4.3 Abstractions to keep swaps cheap

```
trait VectorStore   { upsert, query(filter, k), delete }       // pgvector | Qdrant | LanceDB
trait LexicalIndex  { index(doc), search(query, kind), delete } // Tantivy | Postgres FTS
```

---

## 5. Data Model

### 5.1 Card formats

**L0 card** (target ≤ 600 tokens, stored as JSONB and rendered to compact Markdown for agents)

```yaml
id: proj_01H...
name: ble-sensor-app
one_liner: Flutter app that reads BLE heart-rate sensors and syncs offline-first to Firebase.
status: abandoned            # active | finished | abandoned | archived | recovered
last_commit: 2023-04-11
stack: [flutter, dart, riverpod, firebase, drift]
entry_points: [lib/main.dart, lib/core/ble/scanner.dart]
ideas:                        # the reusable value
  - name: Offline-first sync queue with conflict resolution
    where: lib/sync/queue.dart:12-210
  - name: BLE reconnect with exponential backoff
    where: lib/core/ble/reconnect.dart
reusable_modules: [lib/sync, lib/core/ble]
gotchas: [iOS background BLE needs bluetooth-central capability, Firebase rules never finished]
why_stopped: lost interest after sensor vendor changed protocol
links: {repo: github.com/ansgrb/..., local: ~/code/ble-sensor-app}
card_version: 3
source_commit: 9f3c2ab
```

**L1 map** (queried on demand, never loaded whole)
- File tree with one-line role per file.
- Symbols: classes, functions, exports, with file and line.
- Import / dependency graph (internal and external).
- Dependency manifest with versions.
- Module summaries (about 50–100 tokens each).
- Idea records linked to files.

**L2 pointer:** `path:startLine-endLine@commit`. Agents read the exact range from disk or git, never from the DB.

### 5.2 Postgres schema (initial, superseded)

> [!WARNING]
> **Superseded by §15.4.** This is kept to show how the design evolved. Do not scaffold from it: it has nullable foreign keys, no FK on `edge.dst_file`, commit-based caching, and no `root`, `card`, `embedding_model` or `audit_log` tables. The authoritative `migrations/00001_init.sql` is in §15.4.

```sql
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

CREATE TABLE project (
  id            TEXT PRIMARY KEY,           -- ULID
  name          TEXT NOT NULL,
  root_uri      TEXT NOT NULL,              -- local path or git URL
  status        TEXT NOT NULL,
  languages     TEXT[] NOT NULL DEFAULT '{}',
  stack         TEXT[] NOT NULL DEFAULT '{}',
  last_commit   TEXT,
  last_scanned  TIMESTAMPTZ,
  card          JSONB,                      -- L0
  card_tokens   INT,
  card_version  INT NOT NULL DEFAULT 0
);

CREATE TABLE file (
  id          BIGSERIAL PRIMARY KEY,
  project_id  TEXT REFERENCES project(id) ON DELETE CASCADE,
  path        TEXT NOT NULL,
  lang        TEXT,
  role        TEXT,                         -- one-line purpose
  content_hash TEXT NOT NULL,
  loc         INT,
  UNIQUE (project_id, path)
);
CREATE INDEX file_path_trgm ON file USING gin (path gin_trgm_ops);

CREATE TABLE symbol (
  id         BIGSERIAL PRIMARY KEY,
  file_id    BIGINT REFERENCES file(id) ON DELETE CASCADE,
  name       TEXT NOT NULL,
  kind       TEXT NOT NULL,                 -- class | fn | method | const | type
  start_line INT, end_line INT,
  signature  TEXT
);
CREATE INDEX symbol_name_trgm ON symbol USING gin (name gin_trgm_ops);

CREATE TABLE edge (                          -- dependency / import graph
  src_file BIGINT REFERENCES file(id) ON DELETE CASCADE,
  dst_file BIGINT,                           -- NULL when external
  dst_ext  TEXT,                             -- external package
  kind     TEXT NOT NULL                     -- import | call | extends
);

CREATE TABLE idea (
  id          BIGSERIAL PRIMARY KEY,
  project_id  TEXT REFERENCES project(id) ON DELETE CASCADE,
  title       TEXT NOT NULL,
  summary     TEXT NOT NULL,
  tags        TEXT[] DEFAULT '{}',
  pointers    JSONB NOT NULL,                -- [{path, start, end, commit}]
  embedding   vector(1024)
);
CREATE INDEX idea_embedding_hnsw ON idea USING hnsw (embedding vector_cosine_ops);

CREATE TABLE chunk (                         -- optional L1 semantic chunks
  id         BIGSERIAL PRIMARY KEY,
  file_id    BIGINT REFERENCES file(id) ON DELETE CASCADE,
  start_line INT, end_line INT,
  summary    TEXT,
  embedding  vector(1024)
);

CREATE TABLE scan_run (
  id BIGSERIAL PRIMARY KEY, project_id TEXT, commit TEXT,
  started_at TIMESTAMPTZ, finished_at TIMESTAMPTZ,
  llm_tokens_in INT, llm_tokens_out INT, status TEXT
);
```

Embedding dimension depends on the model chosen in Phase 3. Lock it per index and store the model name in a `meta` table so mixed vectors can't be queried together.

---

## 6. Architecture

```
            ┌───────────────┐
 repos ───► │  Discovery    │  find repos (local dirs, GitHub API, archives)
            └──────┬────────┘
                   ▼
            ┌───────────────┐
            │  Scanner      │  tree-sitter, manifests, git log, secret scan   (Go)
            └──────┬────────┘
                   ▼
            ┌───────────────┐
            │  Summarizer   │  LLM: L0 card + idea records, cached by fingerprint (Go)
            └──────┬────────┘
                   ▼
   ┌──────────────────────────────────────────┐
   │ Postgres (truth) + pgvector              │
   │   └─► Tantivy index (derived)            │
   │   └─► LanceDB snapshot (derived, opt.)   │
   │   └─► Parquet export → DuckDB (analytics)│
   └──────────────────┬───────────────────────┘
                      ▼
            ┌───────────────┐
            │  MCP server   │  tools below                                    (Go)
            └──────┬────────┘
                   ▼
        Claude Code / other agents

 RE branch:  artifact ─► Decompile ─► Normalize ─► "synthetic repo" ─► Scanner (same path)
```

### Language split
- **Go (core, single binary):** scanner, summarizer/LLM orchestration, embedding calls, MCP server, file watcher, RE adapters. No port step later.
- **Python (optional, outside the core):** eval harness and notebooks only.
- **Rust (only if Phase 4 needs it):** a small Tantivy sidecar. See §15.1.
- See §15 for the reasoning; it supersedes any earlier Rust/Python split.

---

## 7. MCP Tool Surface

| Tool | Purpose | Typical response size |
|---|---|---|
| `list_projects(filter?)` | Names, one-liners, status | ≤ 40 tokens/project |
| `get_card(project)` | L0 card | ≤ 600 tokens |
| `search_ideas(query, filters?, k)` | Semantic search over ideas (pgvector) | ≤ 150 tokens/hit |
| `find_symbol(name, kind?)` | Exact/prefix identifier lookup (Tantivy) | ≤ 60 tokens/hit |
| `find_string(text)` | Error strings, config keys, literals | ≤ 60 tokens/hit |
| `find_path(pattern)` | File path lookup | ≤ 40 tokens/hit |
| `get_module(project, module)` | L1 module summary + deps | ≤ 300 tokens |
| `get_pointer(ref)` | Resolve an indexed `project + path + lines` to code. Sandboxed, redacted, size-capped (§15.2) | ≤ 200 lines |
| `related_projects(project)` | Shared deps/ideas | ≤ 200 tokens |
| `scan(project)` / `rescan_all()` | Trigger indexing | status only |
| `recover(artifact)` | Start RE pipeline (Phase 6) | job id |

Every tool returns a `token_estimate` so agents can budget. Default `k` is 5.

---

## 8. Phased Implementation Plan

Each phase ends in something usable. Don't start the next phase until the exit criteria pass.

### Phase 0 — Foundations (2–3 days)
- Repo layout, license, CI (fmt, lint, tests).
- `docker compose` with Postgres 16 + pgvector.
- Migrations with goose (plain SQL) + pgx, initial schema from §5.2 as amended by §15.4.
- Define card/idea JSON schemas and a golden-sample set of 5 hand-written cards.
- Lock the embedding model and the card LLM now (not in Phase 3); record in `docs/decisions.md` and the `embedding_model` table (§15.1, §15.4).

**Exit:** `make up` gives a migrated DB; schemas validate the 5 sample cards.

### Phase 1 — Scanner MVP (1 week)
- Discovery: walk configured roots, detect repos (`.git`), respect `.gitignore`.
- Detect language and stack from manifests: `pubspec.yaml`, `build.gradle(.kts)`, `Package.swift`, `Cargo.toml`, `package.json`, `go.mod`, `pyproject.toml`.
- Tree-sitter parsing for Dart, Kotlin, Swift, Rust, Go, Python, TS/JS: extract symbols and imports.
- Git facts: first/last commit, commit count, authors, last-activity date, tags.
- **Secret scanning pass before anything is stored or sent to an LLM** (see §10).
- Write `project`, `file`, `symbol`, `edge` rows. Incremental by `content_hash`.
- CLI: `necro scan <path>`, `necro scan --all`.

**Exit:** scanning 20 of your real repos completes; re-scan with no changes takes under 2 seconds per repo and writes nothing.

### Phase 2 — Summarizer and L0 cards (1 week)
- Build a compact input bundle per repo: README, manifest, file-tree roles, top symbols, git stats, and selected key files (entry points) — hard-capped at a token budget.
- LLM prompt produces the L0 card in the §5.1 schema plus idea records with pointers. Validate against the JSON schema; retry once on failure.
- Cache by content fingerprint, not commit hash (§15.4); re-run only when the fingerprint, prompt version, model or schema version changes.
- Apply the injection defenses in §15.3 before a card is stored.
- Log tokens per scan in `scan_run`.
- CLI: `necro card <project>`; `necro cards --stale`.

**Exit:** all cards ≤ 600 tokens, schema-valid; spot-check 10 cards by hand for accuracy; total cost for the full graveyard recorded.

### Phase 3 — Postgres retrieval + MCP v1 (1 week)
- Embed idea records and (optionally) module-level chunks; HNSW index.
- Hybrid query: metadata filter (status, stack, date) + vector similarity in one SQL.
- Trigram search on paths and symbol names.
- MCP server exposing `list_projects`, `get_card`, `search_ideas`, `find_symbol`, `find_path`, `get_module`, `get_pointer`.
- Wire it into Claude Code and test with real prompts.

**Exit:** a 20-question eval set (§9) hits ≥ 80% top-3 recall for idea search; typical agent round trip ≤ 1,500 tokens.

### Phase 4 — Tantivy exact-search layer (4–5 days)
- Build the Tantivy index from Postgres: separate fields for identifiers (split camelCase/snake_case, keep the original too), import strings, error strings/literals, and file paths.
- Custom tokenizers: path tokenizer (split on `/` and `.`), identifier tokenizer.
- Incremental updates driven by `scan_run`; full rebuild command.
- Add `find_string`; move `find_symbol` and `find_path` onto Tantivy.
- Keep Postgres trigram as a fallback and for comparison.

**Exit:** exact lookups p95 < 50 ms; benchmark shows precision gains over trigram on the eval set.

### Phase 5 — Knowledge graph, analytics and sync (1 week)
- Cross-project links: shared dependencies, similar ideas (embedding neighbors), module reuse candidates.
- `related_projects` tool.
- Nightly Parquet export + DuckDB queries/dashboards ("what do I keep rebuilding?", "stack usage over time").
- File watcher / git hook for auto-rescan on commit.
- GitHub sync for remote-only repos (clone shallow to a cache).
- Optional: LanceDB snapshot export so a project card pack can travel with a repo.

**Exit:** `necro report` outputs your top reused ideas and abandoned-but-valuable projects.

### Phase 6 — Reverse and rebuild pipeline (3–4 weeks)
Treat this as a separate product built on the same core. Only run it on software you own or are authorized to analyze.

**6a. Intake and fingerprinting**
- Identify artifact type: APK/AAB, IPA, JS bundle/source map, ELF/Mach-O/PE, Flutter AOT, Rust binary.
- Extract metadata: package ids, versions, permissions, assets, embedded resources, signing info, strings.
- Check for lifelines: source maps, ProGuard/R8 `mapping.txt`, debug symbols, build metadata, old CI artifacts.

**6b. Per-target recovery adapters**

| Target | Tooling | Expected recovery |
|---|---|---|
| Android Java/Kotlin | jadx (and apktool for resources/manifest) | High: readable Java, resources, manifest; names better with R8 mapping |
| Web/JS | source maps → original sources; otherwise prettify + deobfuscate | Near-complete with maps; moderate without |
| Flutter (release) | Blutter / reFlutter for AOT snapshots; asset extraction | Structure, class/function names, strings, assets; not clean Dart |
| iOS Swift/ObjC | class-dump / Ghidra, symbols if unstripped | Interfaces and structure; logic needs manual work |
| Native (C/C++/Rust) | Ghidra (headless), `object`/`goblin`, `capstone` | Pseudocode and call graph; noisy |

Adapters wrap the external tools, run them in a sandbox (container, no network), and write output into a normalized folder.

**6c. Normalize into a synthetic repo**
- Output folder with recovered sources, resources, assets and a `RECOVERY.md` noting confidence per file (`exact | decompiled | inferred | missing`).
- Rename obfuscated symbols using R8 mappings, string cross-references, and LLM-suggested names (flagged as suggestions, never silently applied).

**6d. Map and plan**
- Run the Phase 1–2 scanner on the synthetic repo to produce L0/L1.
- Generate a **rewrite plan**: module list, public API surface, data models, network endpoints and payloads (from strings/resources), permissions, third-party SDKs, screen/route inventory, and risk list.
- Generate a **skeleton project** in the target stack (e.g., Flutter or Kotlin) with the module layout, models and stubbed functions, each stub annotated with its recovered pointer.

**6e. Behavior verification (optional, later)**
- Dynamic traces from running the original release (API calls, screens visited) to produce a spec the rewrite must match.
- Golden-behavior tests derived from traces.

**Exit:** on two of your own apps (one Android/Kotlin or web, one Flutter), the pipeline yields a synthetic repo, a rewrite plan, and a skeleton in under 30 minutes of unattended time, with you able to continue development from it.

### Phase 7 — Hardening and polish (ongoing)
- Auth and read-only mode for the MCP server; per-project allow/deny lists.
- Backup/restore (pg_dump + index rebuild), versioned migrations.
- Config file (`necro.toml`), shell completions, good error messages.
- Documentation and a short demo; publish the non-sensitive parts on your portfolio.
- Evaluate Qdrant if pgvector latency or recall degrades.

---

## 9. Evaluation and Success Metrics

Build `eval/questions.yaml` in Phase 2 and grow it:

- 20+ real questions ("which project had my BLE reconnect logic?", "where did I handle token refresh?") with expected projects/files.
- **Recall@3** for idea search; **precision@5** for exact search.
- **Tokens per answered question** (target ≤ 1,500) vs. the baseline of an agent reading the repo directly.
- **Card accuracy**: manual score (1–5) on 10 sampled cards per release.
- **Freshness**: percent of projects whose card matches HEAD.
- **Cost**: total LLM spend per full re-scan.

Re-run the eval on every change to prompts, embedding model or index configuration.

---

## 10. Security and Privacy (important when scanning your whole history)

- **Secret scanning before indexing or LLM calls** (gitleaks rules as a Go library, plus entropy checks). Redact matches; never store raw secrets in DB, cards, embeddings or the exact-search index. Report them so you can rotate them. Redaction is **one pipeline stage** that every sink consumes (DB, search index, LLM bundle, embeddings). Sinks never run their own check.
- **Redact on egress too:** every MCP response that contains source text is re-scanned at read time (§15.2.4).
- Per-project flags: `private`, `no_llm` (index locally only), `skip`. Card quarantine is per-card state, not a flag (§15.4).
- Prefer local embedding models for private repos (default: bge-m3 on local Ollama); document exactly what leaves the machine in `docs/decisions.md`.
- **MCP server uses stdio transport** (no network listener), read-only by default; mutating tools (`scan`, `rescan_all`, `recover`) are disabled unless enabled in config, and `scan` only accepts paths under registered roots. All code-returning tools follow §15.2.
- **`necro forget <project>`** purges a project from Postgres *and* every derived index/cache in one command, for when a secret slips through.
- Test fixtures with planted fake secrets live under `testdata/` and are allowlisted by path in `.gitleaks.toml` (`[allowlist] paths`, extending the default rules), so CI's repo-level gitleaks run stays meaningful everywhere else. The scanner's own tests load the default rules without that allowlist.
- RE sandbox: containers with `--network=none`, `--read-only`, `--cap-drop=ALL`, default seccomp, non-root user, read-only input mounts, CPU/memory/time limits.
- Legal: reverse engineering only for your own software or where you have clear authorization.
- Encrypt backups (e.g. `pg_dump | age -r <recipient>`); keep the Postgres volume off shared drives; Postgres and Ollama bind to `127.0.0.1` only.

---

## 11. Repository Layout

```
necropolis/
├─ README.md
├─ go.mod
├─ cmd/
│  └─ necro/          single binary: CLI subcommands + `necro mcp` (stdio server)
├─ internal/
│  ├─ config/         necro.toml loading, roots, flags, denylist
│  ├─ discovery/      walk roots, detect repos, .gitignore, dedupe by root_commit
│  ├─ scanner/        manifests, git facts, fingerprint + stat cache
│  ├─ parse/          tree-sitter (cgo), tags.scm / import queries, vendored grammars
│  ├─ redact/         gitleaks library + entropy; the ONE redaction stage for every sink
│  ├─ gitx/           hardened git invocation (SHA-only, fixed args, timeouts) (§15.2.7)
│  ├─ sandbox/        os.Root-based file access, denylist enforcement (§15.2)
│  ├─ store/          pgx repository layer
│  ├─ llm/            LLMProvider interface + Gemini implementation
│  ├─ embed/          Embedder interface + Ollama (bge-m3) implementation
│  ├─ summarizer/     bundle builder, prompts, schema validation, card lint (§15.3)
│  ├─ mcp/            tool handlers, provenance envelope, caps, audit log
│  ├─ search/         Phase 4 exact-search backend (chosen by benchmark)
│  └─ recover/        Phase 6 RE adapters (sandboxed containers)
├─ migrations/        goose plain-SQL migrations (00001_init.sql = §15.4)
├─ schemas/           card.schema.json, idea.schema.json, samples/
├─ testdata/          planted-secret repos, injection corpus, traversal fixtures
├─ docs/              decisions.md, card-schema.md, prompts/
├─ eval/              questions.yaml (+ optional Python notebooks/harness)
├─ docker-compose.yml postgres16+pgvector, ollama (both bound to 127.0.0.1)
├─ Makefile
└─ necro.toml.example
```

---

## 12. Risks and Mitigations

| Risk | Mitigation |
|---|---|
| Cards become stale or wrong | Content-fingerprint cache (§15.4), `--stale` rescan, accuracy spot-checks, `prompt_version` / `schema_version` in the cache key |
| LLM cost creep | Token budgets per bundle, caching, tiering (cheap model for bundles, better model for cards), cost logging |
| Over-engineering the stack | Phase gates: exact-search engine only in Phase 4 after a benchmark, Qdrant only if measured need |
| Embedding model change invalidates vectors | `embedding_model` table with one active model; `embedding_model_id` on every vector; re-embed job; never mix |
| Decompiler output is poor (esp. Flutter AOT) | Set expectations: structure + plan, not source; show confidence per file; check Blutter's supported Dart version early |
| Secrets leak into index/LLM/agent | One redaction stage feeding every sink, redact-on-egress for every tool response (§15.2.4), `no_llm` flag, local embeddings, `necro forget` purge |
| Path traversal / git argument injection via MCP | Indexed-files-only pointers, `os.Root`, denylist, SHA-only commits, `git cat-file` with fixed args (§15.2) |
| Stored prompt injection via cards | Untrusted-data prompt framing, schema-enforced output, deterministic lint, per-card quarantine, provenance envelope (§15.3) |
| cgo / tree-sitter build friction | Native per-OS CI builds or `zig cc`; grammars vendored and pinned |
| Scope explosion (RE is its own product) | RE is isolated in Phase 6; Phases 0–5 ship value without it |

---

## 13. Suggested Timeline

| Weeks | Phase | Outcome |
|---|---|---|
| 1 | 0–1 | Scanner works on real repos |
| 2–3 | 2–3 | Cards + MCP v1 usable by agents |
| 4 | 4 | Exact search via Tantivy |
| 5 | 5 | Graph, analytics, auto-sync |
| 6–9 | 6 | Reverse & rebuild pipeline |
| ongoing | 7 | Hardening, docs, portfolio write-up |

Phases 0–3 (about 3–4 weeks) deliver the main goal: agents using your old projects without burning tokens.

---

## 14. First Actions (this week)

1. Create the repo and `docker compose` with Postgres + pgvector.
2. List 10–20 real repos as the test corpus (mix of finished, abandoned, Flutter/Kotlin/Swift/Rust).
3. Hand-write 3 L0 cards to lock the schema.
4. Build the Phase 1 scanner against that corpus.
5. Pick one reverse-engineering candidate (an app you lost the source for) so Phase 6 has a real target from the start.
6. Run a language histogram (`scc` or `tokei`) over your code roots to confirm which grammars to support first (§15.1).

---

## 15. Revision 2: Resolved Decisions and Hardening (authoritative)

Where this section conflicts with anything above, this section wins.

### 15.1 Decisions

| Topic | Decision | Notes |
|---|---|---|
| Core language | **Go** | Single binary, pgx, official MCP SDK (`github.com/modelcontextprotocol/go-sdk`), `os.Root` for path sandboxing. Matches the Go + Python focus for security work. Drops the "prototype in Python, port to Rust" plan, since a port is wasted work. **Cross-compiling is not "easy":** tree-sitter brings in cgo (`github.com/tree-sitter/go-tree-sitter`, with grammars vendored). Release builds run natively on each OS in a CI matrix, or use `zig cc` as the cross C toolchain. |
| Migrations | goose, plain SQL | One authoritative `00001_init.sql` (§15.4). |
| Card LLM | **Gemini, paid tier (or Vertex AI) from day one**, behind an `LLMProvider` interface | SDK `google.golang.org/genai`. Card output is enforced through `ResponseSchema` (structured output) *and* re-validated locally. Pin the exact model id in each card row. A free/local provider can be added later without touching the pipeline. |
| Embeddings | `Embedder` interface; **bge-m3 (1024-dim) served by Ollama** | Runs as a container in `docker-compose`, bound to `127.0.0.1`, reached over HTTP. Nothing leaves the machine, so it's safe for `private` repos. Registered as the single active row in `embedding_model`. Hugging Face TEI is a drop-in alternative behind the same interface. Never mix models in one index. |
| Secret scanning | **gitleaks as a Go library** (`github.com/zricethezav/gitleaks/v8/detect`), version pinned | Runs in-process, because redact-on-egress (§15.2.4) runs on every tool call and launching a CLI each time is too slow. The library API isn't stable, so pin it and cover it with the planted-secret tests. Add an entropy pass for literals. |
| MCP transport | **stdio** | No network listener at all. HTTP transport only if a remote agent ever needs it, and then with auth and a localhost bind. |
| Exact search (Phase 4) | Benchmark three options, pick one | (a) ParadeDB `pg_search`, BM25 inside Postgres, removes the sync problem; note it is **AGPL** (fine for personal use) and needs the ParadeDB Postgres image, which is a one-line compose change; (b) a small Rust Tantivy sidecar; (c) Bleve, pure Go. Verify the current state of each before choosing. |
| Languages first | Dart, Kotlin, Swift, plus Rust/Go/Python/TS | Confirm with the histogram in §14.6. Grammars are pluggable, so the order only affects what ships first. Symbols are extracted through each grammar's `tags.scm` queries, so one generic extractor covers definitions; imports need small per-language queries. |

**Gemini privacy:** the free tier may use inputs to improve the provider's products, so it is **not used**, not even in development. Use the paid API or Vertex AI and re-check the terms whenever the tier changes. Every LLM call is still treated as an egress point: redaction runs first, and `no_llm` projects never reach the provider.

### 15.2 `get_pointer` hardening (and all code-returning tools)

1. **No caller-supplied paths.** Accept only `(project_id, file_id | relpath, start_line, end_line)` where the file exists in the `file` table. Anything not indexed is rejected.
2. **Sandbox to the project root.** Open through Go's `os.OpenRoot` on the registered project root, or `git show <commit>:<path>` for committed content. Reject `..`, absolute paths, and symlinks that leave the root. Resolve and re-check before reading.
3. **Denylist** regardless of index state: `.env*`, `*.pem`, `*.key`, `id_rsa*`, `.ssh/`, `.aws/`, `.git/`, keystores, `*.p12`, `google-services.json`, `.npmrc`, `.netrc`, and similar. Maintain it in config.
4. **Redact on egress.** Run the secret scanner over every returned snippet at read time, not only at index time. Replace matches with `[REDACTED:type]`. The same applies to `get_module`, `find_string`, and any tool that returns source text.
5. **Caps:** max 200 lines per call, max N calls per session, and an audit log of every call (who, what, when).
6. **Tests (required before Phase 3 exits):** `../../.ssh/id_rsa`, absolute paths, symlink escape, a file containing a planted fake AWS key, a request for an unindexed file, and the git argument-injection cases in item 7 must all fail or return redacted output.
7. **No argument injection into git.** A caller-supplied commit is a classic injection point: `git show --output=/home/you/.bashrc` turns a read into a file write, and values like `--exec`, `-c core.pager=…` or a ref named `--help` behave in similar ways.
   - Accept a commit only if it matches `^[0-9a-f]{40}$|^[0-9a-f]{64}$` (SHA-1 / SHA-256). Never accept branch names, `HEAD~n` or other revision syntax from agents. Resolve those server-side if they're needed at all.
   - Confirm that the object exists and is a commit (`git cat-file -t <sha>` must equal `commit`) before using it.
   - Prefer `git cat-file blob <sha>:<path>`, which has no `--output` style options, over `git show`. Build the arguments with `exec.CommandContext`, never through a shell. Pin `-c core.pager=cat -c core.fsmonitor= -c protocol.allow=never`, and set the `GIT_CONFIG_NOSYSTEM=1` and `GIT_TERMINAL_PROMPT=0` environment variables.
   - The path part comes from the `file` table (item 1), never from the request. A timeout and an output size cap apply to every git invocation.

### 15.3 Prompt-injection defenses for cards

Repo content, READMEs, comments and decompiled third-party strings are untrusted input. A card that carries an injected instruction becomes trusted-looking context in every later session.

1. **Exclude by default:** vendored/third-party directories, `node_modules`, `Pods`, decompiled SDK code, and generated files never enter the summarizer bundle. Include them only by explicit opt-in.
2. **Prompt structure:** untrusted content sits inside clearly delimited data blocks; the system prompt states that it is data and that any instructions inside it must be ignored and reported.
3. **Constrained output:** strict JSON schema with enums where possible, hard length caps per field, no free-form "instructions" or "notes to the agent" fields.
4. **Deterministic lint after generation:** reject or quarantine cards whose fields contain instruction-like patterns ("ignore previous", "you must", "system prompt", tool names, shell commands), URLs outside an allowlist, or base64-like blobs. Quarantined cards are not served until reviewed (`necro review`).
   - **False positives:** developer gotchas often contain legitimate commands (`flutter pub get`, `xcrun simctl …`). Commands go into a typed `commands: [string]` card field (≤ 5 entries, ≤ 120 chars each). That field is checked against a denylist (pipes to a shell, `curl … | sh`, `rm -rf`, redirection into dotfiles, encoded payloads) and is always rendered as an inert code block, never as prose. Shell-command patterns are only flagged in the prose fields. Measure the quarantine rate on the real corpus during Phase 2 and tune from that (§15.5).
5. **Provenance envelope:** every MCP response wraps card content as `untrusted_derived_data: true`, with the source fingerprint and a note that nothing inside is an instruction. Agents should be told this in the server's tool descriptions.
6. **Free-text fields are the weak point** (`why_stopped`, `gotchas`, idea summaries). Keep them short and plain; consider structured tags instead of prose where possible.
7. **Synthetic (reverse-engineered) repos** get the strictest mode: bundles built from strings only after filtering, and cards always start in quarantine.
8. **Tests:** a small corpus of repos with planted injections in README, comments, string constants and filenames; none may survive into a served card.

This reduces risk but cannot reach zero, so the provenance envelope and the quarantine step matter as much as the lint.

### 15.4 Fingerprint caching and schema changes

Commit hash is not a valid cache key for dirty working trees, non-git folders, or synthetic repos.

- **Fingerprint** = SHA-256 over the sorted list of `(relpath, content_hash)` for every non-ignored file (tracked and untracked), plus the scanner config version.
- **Stat cache (required for the <2 s no-op rescan):** keep `(mtime_ns, size, inode)` per file next to its `content_hash`, the same way git's index does. Re-hash a file only when its stat tuple changes, then recompute the fingerprint from the cached hashes. Go further for clean git repos: when `HEAD` and the `git status --porcelain` output are both unchanged since the last scan, skip the filesystem walk.
- **Synthetic repos:** artifact SHA-256 + adapter name/version + toolchain versions + recovery config.
- **Cache key** = `(fingerprint, prompt_version, model_id, schema_version)`.
- The git commit is display metadata only.

**Authoritative initial schema (`migrations/00001_init.sql`).** This replaces §5.2 and the earlier `ALTER TABLE` sketch. The project is new, so there is one init migration rather than a base migration plus patches.

```sql
-- +goose Up
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- Registered scan roots. Path sandboxing (§15.2) is anchored here.
CREATE TABLE root (
  id         TEXT PRIMARY KEY,                     -- ULID
  path       TEXT NOT NULL UNIQUE,                 -- absolute, canonicalized at registration
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE project (
  id             TEXT PRIMARY KEY,                 -- ULID
  root_id        TEXT NOT NULL REFERENCES root(id) ON DELETE RESTRICT,
  name           TEXT NOT NULL,
  rel_path       TEXT NOT NULL,                    -- relative to root.path; never absolute
  remote_url     TEXT,
  root_commit    TEXT,                             -- first commit SHA; dedupes duplicate clones
  source_kind    TEXT NOT NULL DEFAULT 'git_clean'
                 CHECK (source_kind IN ('git_clean','git_dirty','plain_dir','synthetic')),
  status         TEXT NOT NULL DEFAULT 'unknown'
                 CHECK (status IN ('unknown','active','finished','abandoned','archived','recovered')),
  status_source  TEXT NOT NULL DEFAULT 'deterministic'
                 CHECK (status_source IN ('deterministic','llm','human')),
  flags          TEXT[] NOT NULL DEFAULT '{}'      -- user config only: private | no_llm | skip
                 CHECK (flags <@ ARRAY['private','no_llm','skip']::TEXT[]),
  languages      TEXT[] NOT NULL DEFAULT '{}',
  stack          TEXT[] NOT NULL DEFAULT '{}',
  head_commit    TEXT,                             -- display metadata only
  fingerprint    TEXT,
  last_scanned   TIMESTAMPTZ,
  UNIQUE (root_id, rel_path)
);
CREATE INDEX project_root_commit ON project (root_commit);

CREATE TABLE file (
  id           BIGSERIAL PRIMARY KEY,
  project_id   TEXT NOT NULL REFERENCES project(id) ON DELETE CASCADE,
  path         TEXT NOT NULL,                      -- relative to project root
  lang         TEXT,
  role         TEXT,
  content_hash TEXT NOT NULL,
  mtime_ns     BIGINT,                             -- stat cache
  size_bytes   BIGINT,
  inode        BIGINT,
  loc          INT,
  redactions   INT NOT NULL DEFAULT 0,
  UNIQUE (project_id, path)
);
CREATE INDEX file_path_trgm ON file USING gin (path gin_trgm_ops);

CREATE TABLE symbol (
  id         BIGSERIAL PRIMARY KEY,
  file_id    BIGINT NOT NULL REFERENCES file(id) ON DELETE CASCADE,
  name       TEXT NOT NULL,
  kind       TEXT NOT NULL
             CHECK (kind IN ('class','fn','method','const','type','field','enum','interface')),
  start_line INT NOT NULL,
  end_line   INT NOT NULL CHECK (end_line >= start_line),
  signature  TEXT                                  -- post-redaction only
);
CREATE INDEX symbol_file       ON symbol (file_id);
CREATE INDEX symbol_name_exact ON symbol (name text_pattern_ops);  -- exact + prefix
CREATE INDEX symbol_name_trgm  ON symbol USING gin (name gin_trgm_ops);

CREATE TABLE edge (
  id       BIGSERIAL PRIMARY KEY,
  src_file BIGINT NOT NULL REFERENCES file(id) ON DELETE CASCADE,
  dst_file BIGINT REFERENCES file(id) ON DELETE CASCADE,
  dst_ext  TEXT,
  kind     TEXT NOT NULL CHECK (kind IN ('import','call','extends')),
  CHECK ((dst_file IS NULL) <> (dst_ext IS NULL))  -- exactly one target
);
CREATE INDEX edge_src ON edge (src_file);
CREATE INDEX edge_dst ON edge (dst_file);
CREATE INDEX edge_ext ON edge (dst_ext);

-- Exactly one active model at a time; switching models = new row + re-embed job.
CREATE TABLE embedding_model (
  id         SERIAL PRIMARY KEY,
  name       TEXT NOT NULL UNIQUE,                 -- e.g. bge-m3
  provider   TEXT NOT NULL,                        -- e.g. ollama
  dim        INT  NOT NULL,
  active     BOOLEAN NOT NULL DEFAULT false,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX embedding_model_one_active ON embedding_model (active) WHERE active;

CREATE TABLE idea (
  id                 BIGSERIAL PRIMARY KEY,
  project_id         TEXT NOT NULL REFERENCES project(id) ON DELETE CASCADE,
  title              TEXT NOT NULL CHECK (length(title) <= 120),
  summary            TEXT NOT NULL CHECK (length(summary) <= 600),
  tags               TEXT[] NOT NULL DEFAULT '{}',
  pointers           JSONB NOT NULL,               -- [{file_id, start, end, fingerprint}]
  embedding_model_id INT REFERENCES embedding_model(id),
  embedding          vector(1024),                 -- dim locked to bge-m3; new dim = new migration
  CHECK ((embedding IS NULL) = (embedding_model_id IS NULL))
);
CREATE INDEX idea_embedding_hnsw ON idea USING hnsw (embedding vector_cosine_ops);

CREATE TABLE chunk (                               -- optional L1 semantic chunks (Phase 3)
  id                 BIGSERIAL PRIMARY KEY,
  file_id            BIGINT NOT NULL REFERENCES file(id) ON DELETE CASCADE,
  start_line         INT NOT NULL,
  end_line           INT NOT NULL CHECK (end_line >= start_line),
  summary            TEXT,
  embedding_model_id INT REFERENCES embedding_model(id),
  embedding          vector(1024),
  CHECK ((embedding IS NULL) = (embedding_model_id IS NULL))
);
CREATE INDEX chunk_embedding_hnsw ON chunk USING hnsw (embedding vector_cosine_ops);

-- Every generated card. Quarantine is a per-card state, not a project flag.
CREATE TABLE card (
  id             BIGSERIAL PRIMARY KEY,
  project_id     TEXT NOT NULL REFERENCES project(id) ON DELETE CASCADE,
  fingerprint    TEXT NOT NULL,
  prompt_version INT  NOT NULL,
  model_id       TEXT NOT NULL,                    -- exact LLM model id
  schema_version INT  NOT NULL,
  body           JSONB NOT NULL,
  tokens         INT  NOT NULL,
  status         TEXT NOT NULL DEFAULT 'quarantined'
                 CHECK (status IN ('quarantined','approved','rejected')),
  lint_findings  JSONB NOT NULL DEFAULT '[]',
  reviewed_at    TIMESTAMPTZ,
  created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (project_id, fingerprint, prompt_version, model_id, schema_version)
);
CREATE INDEX card_serving ON card (project_id, created_at DESC) WHERE status = 'approved';

CREATE TABLE scan_run (
  id             BIGSERIAL PRIMARY KEY,
  project_id     TEXT NOT NULL REFERENCES project(id) ON DELETE CASCADE,
  fingerprint    TEXT,
  head_commit    TEXT,
  started_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
  finished_at    TIMESTAMPTZ,
  llm_model      TEXT,
  llm_tokens_in  INT,
  llm_tokens_out INT,
  secrets_found  INT NOT NULL DEFAULT 0,
  status         TEXT NOT NULL CHECK (status IN ('running','ok','failed','skipped'))
);
CREATE INDEX scan_run_project ON scan_run (project_id, started_at DESC);

-- Append-only audit of every MCP tool call (§15.2.5).
CREATE TABLE audit_log (
  id         BIGSERIAL PRIMARY KEY,
  at         TIMESTAMPTZ NOT NULL DEFAULT now(),
  client     TEXT,                                 -- MCP clientInfo from initialize
  session_id TEXT NOT NULL,
  tool       TEXT NOT NULL,
  args       JSONB NOT NULL,                       -- post-redaction
  outcome    TEXT NOT NULL CHECK (outcome IN ('ok','denied','error')),
  reason     TEXT
);
-- The application DB role gets INSERT/SELECT only on audit_log, with no UPDATE/DELETE.

CREATE TABLE meta (key TEXT PRIMARY KEY, value JSONB NOT NULL);  -- tokenizer, schema versions

-- +goose Down
DROP TABLE IF EXISTS meta, audit_log, scan_run, card, chunk, idea, embedding_model,
  edge, symbol, file, project, root CASCADE;
```

Notes:
- An "L0 card" is served as the newest `card` row with `status = 'approved'` for the project. The old `project.card*` columns are gone. Synthetic-repo cards stay `quarantined` until `necro review` approves them (§15.3.7). Normal cards are approved automatically when the lint finds nothing.
- `project.rel_path` + `root.path` replaces `root_uri`, so no stored value is ever an absolute path an agent can influence.
- The card field `source_commit` becomes `source_fingerprint` (plus an optional `git_commit`). Phase 1 stores `fingerprint`; Phase 2 reads it.

### 15.5 Additional exit criteria

- **Phase 0:** `goose up` / `goose down` / `goose up` round-trips cleanly in CI against an ephemeral Postgres.
- **Phase 1:** dirty-tree and non-git-folder scans produce stable fingerprints; the secret scanner passes a planted-secret test; a no-op rescan of a 5k-file repo finishes in < 2 s and makes zero content reads (verified through the stat cache counters).
- **Phase 2:** the injection corpus (§15.3.8) is clean; every card carries model id, prompt version and fingerprint; the lint false-positive rate on your real corpus is measured and recorded (§15.3.4).
- **Phase 3:** all §15.2.6 tests pass, including the git argument-injection cases, before the MCP server is connected to any agent.

---

## 16. Revision 3: Review Findings Applied (authoritative)

An external review of Revision 2 checked the plan against the code that will be written from it. Each finding was confirmed valid and is fixed in place above. Where §16 and §15 differ, §16 wins.

| # | Finding | Status | Fixed in |
|---|---|---|---|
| R1 | Pre-§15 sections still described the Rust/Python split, commit-hash caching and a localhost listener; §11 would have been copied straight into the scaffold | Fixed | §10, §11, §12, §5.2 banner |
| R2 | `git show <commit>:<path>` with a caller-supplied commit allowed **argument injection** (e.g. `--output=<file>` turns a read into a file write) | Fixed | §15.2.7 |
| R3 | The fingerprint re-hashed every file, so the <2 s no-op rescan target was unreachable | Fixed: stat cache + git short-circuit | §15.4, §15.5 |
| R4 | The schema was a base migration plus `ALTER`s; the review's fixes (NOT NULL FKs, FK on `edge.dst_file`, edge indexes, CHECKs, FK on `scan_run`, prefix index on `symbol.name`) were missing; `root_id` pointed at no table; embeddings had no `model_id` column; `card_cache` had no constraints | Fixed: one authoritative `00001_init.sql` | §15.4 |
| R5 | `quarantined` was a project flag, but quarantine is a per-card state | Fixed: `card.status` | §15.4 |
| R6 | Nothing said how bge-m3 actually runs from Go | Fixed: Ollama container behind `Embedder` | §15.1 |
| R7 | The Gemini tier was left "check later" even though the free tier may use inputs for training | Fixed: paid tier / Vertex from day one; `ResponseSchema`-enforced output | §15.1 |
| R8 | Redact-on-egress runs on every tool call, so shelling out to the gitleaks CLI is too slow | Fixed: gitleaks imported as a Go library, version pinned | §15.1 |
| R9 | "Easy cross-compile" doesn't hold once tree-sitter brings in cgo | Fixed: native builds or `zig cc` | §15.1 |
| R10 | The instruction lint will flag legitimate shell commands that developers write in gotchas | Mitigated: typed `commands[]` field + measured FP rate | §15.3.4, §15.5 |
| R11 | ParadeDB `pg_search` is AGPL and needs its own Postgres image | Noted; deferred to the Phase 4 benchmark | §15.1 |
| R12 | Agents get no value until week 3 | Optional thin slice documented; timeline unchanged | §16.1 |

### 16.1 Optional week-1 thin slice

If agent value is wanted before week 3, reorder (without changing the architecture): discovery + manifests + git facts + gitleaks → Gemini L0 card → MCP `list_projects` + `get_card` over stdio, all in week 1; tree-sitter symbols in week 2; ideas, embeddings and `search_ideas` in weeks 2–3. Every §15.2 / §15.3 control still applies to the slice.
