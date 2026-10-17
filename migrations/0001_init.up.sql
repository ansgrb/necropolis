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
