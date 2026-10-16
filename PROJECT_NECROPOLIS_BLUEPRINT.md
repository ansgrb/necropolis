# Project Memory Bank -- Implementation Blueprint

Working name: **Necropolis**
Owner: Anas Ghareib
Status: Planning → Phase 0
Last updated: 2026-10-16

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

  | Question type                         | Example                                                 | Engine                                   |
  | ------------------------------------- | ------------------------------------------------------- | ---------------------------------------- |
  | Exact lookup                          | `ERR_CERT_PINNING`, `import dio`, `lib/auth/token.dart` | Tantivy (+ Postgres trigram as fallback) |
  | Semantic / "have I done this before?" | "offline sync with conflict resolution"                 | pgvector                                 |
  | Structured / relational               | "all projects using Riverpod and abandoned after 2023"  | Postgres                                 |
  | Analytics                             | "language mix over time", "most reused modules"         | DuckDB over Postgres/Parquet             |

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

| Role                          | Choice                       | Why                                                                                                            |
| ----------------------------- | ---------------------------- | -------------------------------------------------------------------------------------------------------------- |
| **System of record**          | **PostgreSQL 16+**           | Relational integrity, JSONB for cards, transactions, mature tooling                                            |
| **Semantic search**           | **pgvector** (HNSW index)    | Vectors live next to the metadata, so filtered semantic queries are one SQL statement                          |
| **Fuzzy text / paths**        | `pg_trgm` + built-in FTS     | Good enough in Phases 1–3 with zero extra services                                                             |
| **Exact identifier search**   | **Tantivy** (Rust, embedded) | Fast exact/prefix/regex search over identifiers, imports, error strings and file paths using custom tokenizers |
| **Analytics**                 | **DuckDB**                   | Query Parquet exports or attach Postgres for ad-hoc analysis, with no load on the main DB                      |
| **Embedded/portable vectors** | **LanceDB** (optional)       | Local-first mode with no Postgres: columnar, versioned, good for snapshots shipped with a project              |

Postgres is the source of truth. Tantivy and LanceDB are **derived indexes** that can be rebuilt from Postgres at any time. Never write to them first.

### 4.2 Evaluated, and when to use each

| Tool                    | Verdict                                                                                                                                                |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Pinecone                | Skip. Hosted-only, and you don't need a separate vector service at personal scale.                                                                     |
| Milvus                  | Skip until you have tens of millions of vectors. Operationally heavy.                                                                                  |
| Qdrant                  | **Best upgrade path** if pgvector becomes a bottleneck. Great filtering, simple to run. Keep the vector-store interface abstract so swapping is cheap. |
| Chroma                  | Prototyping only. Skip for the real system.                                                                                                            |
| Weaviate                | Skip. Overlaps with Postgres + pgvector, with more moving parts.                                                                                       |
| LanceDB                 | **Optional** embedded mode (see above).                                                                                                                |
| DuckDB                  | **Use** for analytics and Parquet export. Not a primary store.                                                                                         |
| Quickwit                | Skip. Built for distributed log analytics; Tantivy embedded already covers your scale.                                                                 |
| Meilisearch / Typesense | Optional later for a typo-tolerant search UI. Not needed for agent retrieval.                                                                          |

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
status: abandoned # active | finished | abandoned | archived | recovered
last_commit: 2023-04-11
stack: [flutter, dart, riverpod, firebase, drift]
entry_points: [lib/main.dart, lib/core/ble/scanner.dart]
ideas: # the reusable value
  - name: Offline-first sync queue with conflict resolution
    where: lib/sync/queue.dart:12-210
  - name: BLE reconnect with exponential backoff
    where: lib/core/ble/reconnect.dart
reusable_modules: [lib/sync, lib/core/ble]
gotchas:
  [
    iOS background BLE needs bluetooth-central capability,
    Firebase rules never finished,
  ]
why_stopped: lost interest after sensor vendor changed protocol
links: { repo: github.com/ansgrb/..., local: ~/code/ble-sensor-app }
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

### 5.2 Postgres schema (initial)

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
            │  Scanner      │  tree-sitter, manifests, git log, secret scan   (Rust)
            └──────┬────────┘
                   ▼
            ┌───────────────┐
            │  Summarizer   │  LLM: L0 card + idea records, cached by commit  (Python)
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
            │  MCP server   │  tools below                                    (Rust or Python)
            └──────┬────────┘
                   ▼
        Claude Code / other agents

 RE branch:  artifact ─► Decompile ─► Normalize ─► "synthetic repo" ─► Scanner (same path)
```

### Language split

- **Rust:** scanner, Tantivy indexing, file watcher, binary parsing helpers (`goblin`, `object`, `capstone`). Fast, single binary, good portfolio piece.
- **Python:** summarizer/LLM orchestration, embedding calls, glue, eval harness. Fastest to iterate.
- **Reasoning:** prototype in Python everywhere, then move only measured hot paths to Rust.

---

## 7. MCP Tool Surface

| Tool                               | Purpose                                                 | Typical response size |
| ---------------------------------- | ------------------------------------------------------- | --------------------- |
| `list_projects(filter?)`           | Names, one-liners, status                               | ≤ 40 tokens/project   |
| `get_card(project)`                | L0 card                                                 | ≤ 600 tokens          |
| `search_ideas(query, filters?, k)` | Semantic search over ideas (pgvector)                   | ≤ 150 tokens/hit      |
| `find_symbol(name, kind?)`         | Exact/prefix identifier lookup (Tantivy)                | ≤ 60 tokens/hit       |
| `find_string(text)`                | Error strings, config keys, literals                    | ≤ 60 tokens/hit       |
| `find_path(pattern)`               | File path lookup                                        | ≤ 40 tokens/hit       |
| `get_module(project, module)`      | L1 module summary + deps                                | ≤ 300 tokens          |
| `get_pointer(ref)`                 | Resolve `path:lines@commit` to code (reads git, not DB) | by range              |
| `related_projects(project)`        | Shared deps/ideas                                       | ≤ 200 tokens          |
| `scan(project)` / `rescan_all()`   | Trigger indexing                                        | status only           |
| `recover(artifact)`                | Start RE pipeline (Phase 6)                             | job id                |

Every tool returns a `token_estimate` so agents can budget. Default `k` is 5.

---

## 8. Phased Implementation Plan

Each phase ends in something usable. Don't start the next phase until the exit criteria pass.

### Phase 0 — Foundations (2–3 days)

- Repo layout, license, CI (fmt, lint, tests).
- `docker compose` with Postgres 16 + pgvector.
- Migrations tool (sqlx-migrate or Alembic), initial schema from §5.2.
- Define card/idea JSON schemas and a golden-sample set of 5 hand-written cards.
- Pick the embedding model and LLM for summarization; record the choice in `docs/decisions.md`.

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
- Cache by `source_commit` + `card_version`; only re-run when the commit changes or the prompt version bumps.
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

| Target              | Tooling                                                          | Expected recovery                                                      |
| ------------------- | ---------------------------------------------------------------- | ---------------------------------------------------------------------- |
| Android Java/Kotlin | jadx (and apktool for resources/manifest)                        | High: readable Java, resources, manifest; names better with R8 mapping |
| Web/JS              | source maps → original sources; otherwise prettify + deobfuscate | Near-complete with maps; moderate without                              |
| Flutter (release)   | Blutter / reFlutter for AOT snapshots; asset extraction          | Structure, class/function names, strings, assets; not clean Dart       |
| iOS Swift/ObjC      | class-dump / Ghidra, symbols if unstripped                       | Interfaces and structure; logic needs manual work                      |
| Native (C/C++/Rust) | Ghidra (headless), `object`/`goblin`, `capstone`                 | Pseudocode and call graph; noisy                                       |

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

- **Secret scanning before indexing or LLM calls** (gitleaks/trufflehog rules, plus entropy checks). Redact matches; never store raw secrets in DB, cards or embeddings. Report them so you can rotate them.
- Per-project flags: `private`, `no_llm` (index locally only), `skip`.
- Prefer local embedding models for private repos; document exactly what leaves the machine.
- MCP server binds to localhost, read-only by default; mutating tools require explicit config.
- RE sandbox: containers with no network, read-only mounts, resource limits.
- Legal: reverse engineering only for your own software or where you have clear authorization.
- Encrypt backups; keep the Postgres volume off shared drives.

---

## 11. Repository Layout

```
necropolis/
├─ README.md
├─ docs/            decisions.md, card-schema.md, prompts/
├─ migrations/
├─ crates/
│  ├─ scanner/      discovery, tree-sitter, git facts, secret scan
│  ├─ index/        Tantivy builder/query
│  ├─ mcp/          MCP server (or py/ if prototyped in Python)
│  └─ re-adapters/  decompiler wrappers, binary helpers
├─ py/
│  ├─ summarizer/   bundle builder, prompts, validation, cache
│  ├─ embeddings/
│  └─ eval/
├─ schemas/         card.schema.json, idea.schema.json
├─ docker-compose.yml
├─ Makefile
└─ necro.toml.example
```

---

## 12. Risks and Mitigations

| Risk                                         | Mitigation                                                                                                 |
| -------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| Cards become stale or wrong                  | Commit-hash cache, `--stale` rescan, accuracy spot-checks, card_version                                    |
| LLM cost creep                               | Token budgets per bundle, caching, tiering (cheap model for bundles, better model for cards), cost logging |
| Over-engineering the stack                   | Phase gates: Tantivy only in Phase 4, Qdrant only if measured need                                         |
| Embedding model change invalidates vectors   | Store model name/dim; re-embed job; never mix                                                              |
| Decompiler output is poor (esp. Flutter AOT) | Set expectations: structure + plan, not source; show confidence per file                                   |
| Secrets leak into index/LLM                  | Mandatory redaction step, `no_llm` flag, local embeddings                                                  |
| Scope explosion (RE is its own product)      | RE is isolated in Phase 6; Phases 0–5 ship value without it                                                |

---

## 13. Suggested Timeline

| Weeks   | Phase | Outcome                             |
| ------- | ----- | ----------------------------------- |
| 1       | 0–1   | Scanner works on real repos         |
| 2–3     | 2–3   | Cards + MCP v1 usable by agents     |
| 4       | 4     | Exact search via Tantivy            |
| 5       | 5     | Graph, analytics, auto-sync         |
| 6–9     | 6     | Reverse & rebuild pipeline          |
| ongoing | 7     | Hardening, docs, portfolio write-up |

Phases 0–3 (about 3–4 weeks) deliver the main goal: agents using your old projects without burning tokens.

---

## 14. First Actions (this week)

1. Create the repo and `docker compose` with Postgres + pgvector.
2. List 10–20 real repos as the test corpus (mix of finished, abandoned, Flutter/Kotlin/Swift/Rust).
3. Hand-write 3 L0 cards to lock the schema.
4. Build the Phase 1 scanner against that corpus.
5. Pick one reverse-engineering candidate (an app you lost the source for) so Phase 6 has a real target from the start.
