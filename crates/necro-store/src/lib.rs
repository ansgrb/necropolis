//! Postgres + pgvector storage layer (`sqlx` runtime queries, blueprint §5.2 & §15.4).

use anyhow::Context;
use necro_core::{Card, CardStatus, ProjectStatus, SourceKind, new_id};
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Row, Transaction, Postgres};
use std::collections::HashMap;

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRow {
    pub id: String,
    pub root_id: String,
    pub root_path: String,
    pub name: String,
    pub rel_path: String,
    pub remote_url: Option<String>,
    pub root_commit: Option<String>,
    pub source_kind: String,
    pub status: String,
    pub status_source: String,
    pub flags: Vec<String>,
    pub languages: Vec<String>,
    pub stack: Vec<String>,
    pub head_commit: Option<String>,
    pub fingerprint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileStatRow {
    pub id: i64,
    pub path: String,
    pub content_hash: String,
    pub mtime_ns: Option<i64>,
    pub size_bytes: Option<i64>,
    pub inode: Option<i64>,
    pub lang: Option<String>,
    pub role: Option<String>,
    pub loc: Option<i32>,
    pub redactions: i32,
}

#[derive(Debug, Clone)]
pub struct SymbolInsert {
    pub name: String,
    pub kind: String,
    pub start_line: i32,
    pub end_line: i32,
    pub signature: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FileUpsert {
    pub path: String,
    pub lang: Option<String>,
    pub role: Option<String>,
    pub content_hash: String,
    pub mtime_ns: Option<i64>,
    pub size_bytes: Option<i64>,
    pub inode: Option<i64>,
    pub loc: i32,
    pub redactions: i32,
    pub symbols: Vec<SymbolInsert>,
    pub imports: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolHit {
    pub project_id: String,
    pub project_name: String,
    pub file_path: String,
    pub name: String,
    pub kind: String,
    pub start_line: i32,
    pub end_line: i32,
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathHit {
    pub project_id: String,
    pub project_name: String,
    pub file_id: i64,
    pub path: String,
    pub lang: Option<String>,
    pub role: Option<String>,
    pub loc: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdeaHit {
    pub id: i64,
    pub project_id: String,
    pub project_name: String,
    pub title: String,
    pub summary: String,
    pub tags: Vec<String>,
    pub pointers: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardRecord {
    pub id: i64,
    pub project_id: String,
    pub project_name: String,
    pub fingerprint: String,
    pub prompt_version: i32,
    pub model_id: String,
    pub schema_version: i32,
    pub card: Card,
    pub tokens: i32,
    pub status: String,
    pub lint_findings: serde_json::Value,
}

impl Store {
    pub async fn connect(database_url: &str) -> anyhow::Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(8)
            .connect(database_url)
            .await
            .with_context(|| format!("failed to connect to Postgres at {database_url}"))?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Runs the authoritative `migrations/0001_init.up.sql` inside a transaction (idempotent).
    pub async fn migrate_up(&self) -> anyhow::Result<()> {
        let exists: Option<String> = sqlx::query_scalar("SELECT to_regclass('public.meta')::text")
            .fetch_one(&self.pool)
            .await
            .unwrap_or(None);
        if exists.is_some() {
            return Ok(());
        }
        let sql = include_str!("../../../migrations/0001_init.up.sql");
        sqlx::raw_sql(sql).execute(&self.pool).await.context("migrate_up failed")?;
        Ok(())
    }

    /// Reverts the schema using `migrations/0001_init.down.sql`.
    pub async fn migrate_down(&self) -> anyhow::Result<()> {
        let sql = include_str!("../../../migrations/0001_init.down.sql");
        sqlx::raw_sql(sql).execute(&self.pool).await.context("migrate_down failed")?;
        Ok(())
    }

    /// Registers a canonicalized scan root (or returns the existing row's ID).
    pub async fn ensure_root(&self, canon_path: &str) -> anyhow::Result<String> {
        let id = new_id("root");
        let row = sqlx::query(
            "INSERT INTO root (id, path) VALUES ($1, $2)
             ON CONFLICT (path) DO UPDATE SET path = EXCLUDED.path
             RETURNING id",
        )
        .bind(&id)
        .bind(canon_path)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get("id"))
    }

    /// Looks up a project by name or ID.
    pub async fn get_project(&self, name_or_id: &str) -> anyhow::Result<Option<ProjectRow>> {
        let row = sqlx::query(
            "SELECT p.id, p.root_id, r.path AS root_path, p.name, p.rel_path, p.remote_url,
                    p.root_commit, p.source_kind, p.status, p.status_source, p.flags,
                    p.languages, p.stack, p.head_commit, p.fingerprint
             FROM project p JOIN root r ON r.id = p.root_id
             WHERE p.id = $1 OR p.name = $1
             ORDER BY p.last_scanned DESC NULLS LAST LIMIT 1",
        )
        .bind(name_or_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(map_project_row))
    }

    pub async fn get_project_by_root_rel(&self, root_id: &str, rel_path: &str) -> anyhow::Result<Option<ProjectRow>> {
        let row = sqlx::query(
            "SELECT p.id, p.root_id, r.path AS root_path, p.name, p.rel_path, p.remote_url,
                    p.root_commit, p.source_kind, p.status, p.status_source, p.flags,
                    p.languages, p.stack, p.head_commit, p.fingerprint
             FROM project p JOIN root r ON r.id = p.root_id
             WHERE p.root_id = $1 AND p.rel_path = $2",
        )
        .bind(root_id)
        .bind(rel_path)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(map_project_row))
    }

    pub async fn list_projects(&self, status_filter: Option<&str>, stack_filter: Option<&str>) -> anyhow::Result<Vec<ProjectRow>> {
        let rows = sqlx::query(
            "SELECT p.id, p.root_id, r.path AS root_path, p.name, p.rel_path, p.remote_url,
                    p.root_commit, p.source_kind, p.status, p.status_source, p.flags,
                    p.languages, p.stack, p.head_commit, p.fingerprint
             FROM project p JOIN root r ON r.id = p.root_id
             WHERE ($1::TEXT IS NULL OR p.status = $1)
               AND ($2::TEXT IS NULL OR $2 = ANY(p.stack) OR $2 = ANY(p.languages))
             ORDER BY p.name ASC",
        )
        .bind(status_filter)
        .bind(stack_filter)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(map_project_row).collect())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_project(
        &self,
        root_id: &str,
        name: &str,
        rel_path: &str,
        remote_url: Option<&str>,
        root_commit: Option<&str>,
        source_kind: SourceKind,
        inferred_status: ProjectStatus,
        languages: &[String],
        stack: &[String],
        head_commit: Option<&str>,
        fingerprint: &str,
    ) -> anyhow::Result<String> {
        let new_proj_id = new_id("proj");
        // Preserve human status overrides (`status_source = 'human'`).
        let row = sqlx::query(
            "INSERT INTO project (
                id, root_id, name, rel_path, remote_url, root_commit,
                source_kind, status, status_source, languages, stack,
                head_commit, fingerprint, last_scanned
             ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'deterministic',$9,$10,$11,$12,now())
             ON CONFLICT (root_id, rel_path) DO UPDATE SET
                name = EXCLUDED.name,
                remote_url = COALESCE(EXCLUDED.remote_url, project.remote_url),
                root_commit = COALESCE(EXCLUDED.root_commit, project.root_commit),
                source_kind = EXCLUDED.source_kind,
                status = CASE WHEN project.status_source = 'human' THEN project.status ELSE EXCLUDED.status END,
                languages = EXCLUDED.languages,
                stack = EXCLUDED.stack,
                head_commit = EXCLUDED.head_commit,
                fingerprint = EXCLUDED.fingerprint,
                last_scanned = now()
             RETURNING id",
        )
        .bind(&new_proj_id)
        .bind(root_id)
        .bind(name)
        .bind(rel_path)
        .bind(remote_url)
        .bind(root_commit)
        .bind(source_kind.as_str())
        .bind(inferred_status.as_str())
        .bind(languages)
        .bind(stack)
        .bind(head_commit)
        .bind(fingerprint)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get("id"))
    }

    /// Loads the per-file stat cache for a project so unchanged files skip re-reading/re-hashing.
    pub async fn load_file_stats(&self, project_id: &str) -> anyhow::Result<HashMap<String, FileStatRow>> {
        let rows = sqlx::query(
            "SELECT id, path, content_hash, mtime_ns, size_bytes, inode, lang, role, loc, redactions
             FROM file WHERE project_id = $1",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        let mut map = HashMap::with_capacity(rows.len());
        for r in rows {
            let path: String = r.get("path");
            map.insert(
                path.clone(),
                FileStatRow {
                    id: r.get("id"),
                    path,
                    content_hash: r.get("content_hash"),
                    mtime_ns: r.get("mtime_ns"),
                    size_bytes: r.get("size_bytes"),
                    inode: r.get("inode"),
                    lang: r.get("lang"),
                    role: r.get("role"),
                    loc: r.get("loc"),
                    redactions: r.get("redactions"),
                },
            );
        }
        Ok(map)
    }

    /// Incrementally applies modified/added files and removes deleted files inside one transaction.
    pub async fn apply_file_changes(
        &self,
        project_id: &str,
        changed: &[FileUpsert],
        keep_paths: &[String],
    ) -> anyhow::Result<()> {
        let mut tx: Transaction<'_, Postgres> = self.pool.begin().await?;

        sqlx::query("DELETE FROM file WHERE project_id = $1 AND NOT (path = ANY($2))")
            .bind(project_id)
            .bind(keep_paths)
            .execute(&mut *tx)
            .await?;

        for f in changed {
            let row = sqlx::query(
                "INSERT INTO file (project_id, path, lang, role, content_hash, mtime_ns, size_bytes, inode, loc, redactions)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)
                 ON CONFLICT (project_id, path) DO UPDATE SET
                    lang = EXCLUDED.lang,
                    role = EXCLUDED.role,
                    content_hash = EXCLUDED.content_hash,
                    mtime_ns = EXCLUDED.mtime_ns,
                    size_bytes = EXCLUDED.size_bytes,
                    inode = EXCLUDED.inode,
                    loc = EXCLUDED.loc,
                    redactions = EXCLUDED.redactions
                 RETURNING id",
            )
            .bind(project_id)
            .bind(&f.path)
            .bind(&f.lang)
            .bind(&f.role)
            .bind(&f.content_hash)
            .bind(f.mtime_ns)
            .bind(f.size_bytes)
            .bind(f.inode)
            .bind(f.loc)
            .bind(f.redactions)
            .fetch_one(&mut *tx)
            .await?;
            let file_id: i64 = row.get("id");

            sqlx::query("DELETE FROM symbol WHERE file_id = $1").bind(file_id).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM edge WHERE src_file = $1").bind(file_id).execute(&mut *tx).await?;

            for s in &f.symbols {
                sqlx::query(
                    "INSERT INTO symbol (file_id, name, kind, start_line, end_line, signature)
                     VALUES ($1,$2,$3,$4,$5,$6)",
                )
                .bind(file_id)
                .bind(&s.name)
                .bind(&s.kind)
                .bind(s.start_line)
                .bind(s.end_line)
                .bind(&s.signature)
                .execute(&mut *tx)
                .await?;
            }

            for imp in &f.imports {
                sqlx::query("INSERT INTO edge (src_file, dst_ext, kind) VALUES ($1,$2,'import')")
                    .bind(file_id)
                    .bind(imp)
                    .execute(&mut *tx)
                    .await?;
            }
        }

        tx.commit().await?;
        Ok(())
    }

    pub async fn record_scan_run(
        &self,
        project_id: &str,
        fingerprint: &str,
        head_commit: Option<&str>,
        secrets_found: i32,
        status: &str,
    ) -> anyhow::Result<i64> {
        let row = sqlx::query(
            "INSERT INTO scan_run (project_id, fingerprint, head_commit, finished_at, secrets_found, status)
             VALUES ($1,$2,$3,now(),$4,$5) RETURNING id",
        )
        .bind(project_id)
        .bind(fingerprint)
        .bind(head_commit)
        .bind(secrets_found)
        .bind(status)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.get("id"))
    }

    /// Saves a generated card and replaces the project's `idea` records when the card is approved.
    pub async fn upsert_card(
        &self,
        project_id: &str,
        card: &Card,
        prompt_version: i32,
        model_id: &str,
        schema_version: i32,
        tokens: i32,
        status: CardStatus,
        lint_findings: &[String],
    ) -> anyhow::Result<i64> {
        let body = serde_json::to_value(card)?;
        let findings_json = serde_json::to_value(lint_findings)?;
        let mut tx = self.pool.begin().await?;

        let row = sqlx::query(
            "INSERT INTO card (
                project_id, fingerprint, prompt_version, model_id, schema_version,
                body, tokens, status, lint_findings, reviewed_at
             ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9, CASE WHEN $8 = 'approved' THEN now() ELSE NULL END)
             ON CONFLICT (project_id, fingerprint, prompt_version, model_id, schema_version)
             DO UPDATE SET
                body = EXCLUDED.body,
                tokens = EXCLUDED.tokens,
                status = EXCLUDED.status,
                lint_findings = EXCLUDED.lint_findings,
                reviewed_at = EXCLUDED.reviewed_at
             RETURNING id",
        )
        .bind(project_id)
        .bind(&card.source_fingerprint)
        .bind(prompt_version)
        .bind(model_id)
        .bind(schema_version)
        .bind(&body)
        .bind(tokens)
        .bind(status.as_str())
        .bind(&findings_json)
        .fetch_one(&mut *tx)
        .await?;
        let card_id: i64 = row.get("id");

        if status == CardStatus::Approved {
            sqlx::query("DELETE FROM idea WHERE project_id = $1").bind(project_id).execute(&mut *tx).await?;
            for idea in &card.ideas {
                let ptr = serde_json::json!([{
                    "where": idea.location,
                    "fingerprint": card.source_fingerprint,
                }]);
                let title: String = idea.name.chars().take(120).collect();
                let summary = format!("{} ({})", card.one_liner, idea.location);
                let summary_trunc: String = summary.chars().take(600).collect();
                sqlx::query(
                    "INSERT INTO idea (project_id, title, summary, tags, pointers)
                     VALUES ($1,$2,$3,$4,$5)",
                )
                .bind(project_id)
                .bind(title)
                .bind(summary_trunc)
                .bind(&card.stack)
                .bind(ptr)
                .execute(&mut *tx)
                .await?;
            }
        }

        tx.commit().await?;
        Ok(card_id)
    }

    pub async fn get_serving_card(&self, project_id: &str) -> anyhow::Result<Option<CardRecord>> {
        let row = sqlx::query(
            "SELECT c.id, c.project_id, p.name AS project_name, c.fingerprint, c.prompt_version,
                    c.model_id, c.schema_version, c.body, c.tokens, c.status, c.lint_findings
             FROM card c JOIN project p ON p.id = c.project_id
             WHERE c.project_id = $1 AND c.status = 'approved'
             ORDER BY c.created_at DESC LIMIT 1",
        )
        .bind(project_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(map_card_row).transpose()
    }

    pub async fn list_quarantined_cards(&self) -> anyhow::Result<Vec<CardRecord>> {
        let rows = sqlx::query(
            "SELECT c.id, c.project_id, p.name AS project_name, c.fingerprint, c.prompt_version,
                    c.model_id, c.schema_version, c.body, c.tokens, c.status, c.lint_findings
             FROM card c JOIN project p ON p.id = c.project_id
             WHERE c.status = 'quarantined'
             ORDER BY c.created_at DESC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(map_card_row).collect()
    }

    pub async fn set_card_status(&self, card_id: i64, status: CardStatus) -> anyhow::Result<bool> {
        let res = sqlx::query("UPDATE card SET status = $2, reviewed_at = now() WHERE id = $1")
            .bind(card_id)
            .bind(status.as_str())
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Exact + prefix + trigram symbol lookup across all projects.
    pub async fn find_symbols(&self, query: &str, kind: Option<&str>, limit: i64) -> anyhow::Result<Vec<SymbolHit>> {
        let pattern = format!("{query}%");
        let sub = format!("%{query}%");
        let rows = sqlx::query(
            "SELECT p.id AS project_id, p.name AS project_name, f.path AS file_path,
                    s.name, s.kind, s.start_line, s.end_line, s.signature
             FROM symbol s
             JOIN file f ON f.id = s.file_id
             JOIN project p ON p.id = f.project_id
             WHERE ($2::TEXT IS NULL OR s.kind = $2)
               AND (s.name = $1 OR s.name LIKE $3 OR s.name ILIKE $4)
             ORDER BY (s.name = $1) DESC, (s.name LIKE $3) DESC, length(s.name) ASC
             LIMIT $5",
        )
        .bind(query)
        .bind(kind)
        .bind(pattern)
        .bind(sub)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| SymbolHit {
                project_id: r.get("project_id"),
                project_name: r.get("project_name"),
                file_path: r.get("file_path"),
                name: r.get("name"),
                kind: r.get("kind"),
                start_line: r.get("start_line"),
                end_line: r.get("end_line"),
                signature: r.get("signature"),
            })
            .collect())
    }

    pub async fn find_paths(&self, pattern: &str, project_filter: Option<&str>, limit: i64) -> anyhow::Result<Vec<PathHit>> {
        let sub = format!("%{pattern}%");
        let rows = sqlx::query(
            "SELECT p.id AS project_id, p.name AS project_name, f.id AS file_id,
                    f.path, f.lang, f.role, f.loc
             FROM file f JOIN project p ON p.id = f.project_id
             WHERE f.path ILIKE $1
               AND ($2::TEXT IS NULL OR p.id = $2 OR p.name = $2)
             ORDER BY length(f.path) ASC
             LIMIT $3",
        )
        .bind(sub)
        .bind(project_filter)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| PathHit {
                project_id: r.get("project_id"),
                project_name: r.get("project_name"),
                file_id: r.get("file_id"),
                path: r.get("path"),
                lang: r.get("lang"),
                role: r.get("role"),
                loc: r.get("loc"),
            })
            .collect())
    }

    /// Resolves a file strictly from the `file` table (`get_pointer` requirement §15.2.1).
    pub async fn get_indexed_file(&self, project_id: &str, rel_path: &str) -> anyhow::Result<Option<FileStatRow>> {
        let row = sqlx::query(
            "SELECT id, path, content_hash, mtime_ns, size_bytes, inode, lang, role, loc, redactions
             FROM file WHERE project_id = $1 AND path = $2",
        )
        .bind(project_id)
        .bind(rel_path)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| FileStatRow {
            id: r.get("id"),
            path: r.get("path"),
            content_hash: r.get("content_hash"),
            mtime_ns: r.get("mtime_ns"),
            size_bytes: r.get("size_bytes"),
            inode: r.get("inode"),
            lang: r.get("lang"),
            role: r.get("role"),
            loc: r.get("loc"),
            redactions: r.get("redactions"),
        }))
    }

    pub async fn search_ideas(&self, query: &str, stack_filter: Option<&str>, limit: i64) -> anyhow::Result<Vec<IdeaHit>> {
        let sub = format!("%{query}%");
        let rows = sqlx::query(
            "SELECT i.id, i.project_id, p.name AS project_name, i.title, i.summary, i.tags, i.pointers,
                    GREATEST(similarity(i.title, $1), similarity(i.summary, $1)) AS sim
             FROM idea i JOIN project p ON p.id = i.project_id
             WHERE ($3::TEXT IS NULL OR $3 = ANY(i.tags) OR $3 = ANY(p.stack))
               AND (i.title ILIKE $2 OR i.summary ILIKE $2 OR similarity(i.title, $1) > 0.12 OR similarity(i.summary, $1) > 0.12)
             ORDER BY sim DESC, i.id DESC
             LIMIT $4",
        )
        .bind(query)
        .bind(sub)
        .bind(stack_filter)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| IdeaHit {
                id: r.get("id"),
                project_id: r.get("project_id"),
                project_name: r.get("project_name"),
                title: r.get("title"),
                summary: r.get("summary"),
                tags: r.get("tags"),
                pointers: r.get("pointers"),
            })
            .collect())
    }

    pub async fn top_symbols_for_project(&self, project_id: &str, limit: i64) -> anyhow::Result<Vec<SymbolHit>> {
        let rows = sqlx::query(
            "SELECT p.id AS project_id, p.name AS project_name, f.path AS file_path,
                    s.name, s.kind, s.start_line, s.end_line, s.signature
             FROM symbol s
             JOIN file f ON f.id = s.file_id
             JOIN project p ON p.id = f.project_id
             WHERE f.project_id = $1
             ORDER BY (s.kind IN ('class','interface','enum','fn')) DESC, f.path ASC, s.start_line ASC
             LIMIT $2",
        )
        .bind(project_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| SymbolHit {
                project_id: r.get("project_id"),
                project_name: r.get("project_name"),
                file_path: r.get("file_path"),
                name: r.get("name"),
                kind: r.get("kind"),
                start_line: r.get("start_line"),
                end_line: r.get("end_line"),
                signature: r.get("signature"),
            })
            .collect())
    }

    pub async fn get_module_summary(&self, project_id: &str, module_prefix: &str) -> anyhow::Result<serde_json::Value> {
        let like = if module_prefix.is_empty() || module_prefix == "." {
            "%".to_string()
        } else {
            format!("{}%", module_prefix.trim_end_matches('/'))
        };
        let files = sqlx::query(
            "SELECT id, path, lang, role, loc FROM file WHERE project_id = $1 AND path LIKE $2 ORDER BY path ASC LIMIT 50",
        )
        .bind(project_id)
        .bind(&like)
        .fetch_all(&self.pool)
        .await?;

        let mut file_items = Vec::new();
        for f in files {
            file_items.push(serde_json::json!({
                "path": f.get::<String, _>("path"),
                "lang": f.get::<Option<String>, _>("lang"),
                "role": f.get::<Option<String>, _>("role"),
                "loc": f.get::<Option<i32>, _>("loc"),
            }));
        }

        let imports = sqlx::query(
            "SELECT DISTINCT e.dst_ext FROM edge e JOIN file f ON f.id = e.src_file
             WHERE f.project_id = $1 AND f.path LIKE $2 AND e.dst_ext IS NOT NULL
             ORDER BY e.dst_ext ASC LIMIT 40",
        )
        .bind(project_id)
        .bind(&like)
        .fetch_all(&self.pool)
        .await?;
        let ext_deps: Vec<String> = imports.into_iter().filter_map(|r| r.get("dst_ext")).collect();

        Ok(serde_json::json!({
            "module": module_prefix,
            "files": file_items,
            "external_imports": ext_deps,
        }))
    }

    pub async fn related_projects(&self, project_id: &str) -> anyhow::Result<Vec<serde_json::Value>> {
        let rows = sqlx::query(
            "SELECT other.id, other.name, other.status,
                    ARRAY(SELECT UNNEST(other.stack) INTERSECT SELECT UNNEST(me.stack)) AS shared_stack
             FROM project me
             JOIN project other ON other.id <> me.id
             WHERE me.id = $1
               AND (other.stack && me.stack OR other.languages && me.languages)
             ORDER BY COALESCE(array_length(ARRAY(SELECT UNNEST(other.stack) INTERSECT SELECT UNNEST(me.stack)), 1), 0) DESC
             LIMIT 10",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.get::<String, _>("id"),
                    "name": r.get::<String, _>("name"),
                    "status": r.get::<String, _>("status"),
                    "shared_stack": r.get::<Vec<String>, _>("shared_stack"),
                })
            })
            .collect())
    }

    /// Purges a project and all cascading records (`necro forget <project>`, blueprint §10).
    pub async fn forget_project(&self, name_or_id: &str) -> anyhow::Result<bool> {
        let res = sqlx::query("DELETE FROM project WHERE id = $1 OR name = $1")
            .bind(name_or_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn record_audit(
        &self,
        client: Option<&str>,
        session_id: &str,
        tool: &str,
        args: &serde_json::Value,
        outcome: &str,
        reason: Option<&str>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO audit_log (client, session_id, tool, args, outcome, reason)
             VALUES ($1,$2,$3,$4,$5,$6)",
        )
        .bind(client)
        .bind(session_id)
        .bind(tool)
        .bind(args)
        .bind(outcome)
        .bind(reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

fn map_project_row(r: sqlx::postgres::PgRow) -> ProjectRow {
    ProjectRow {
        id: r.get("id"),
        root_id: r.get("root_id"),
        root_path: r.get("root_path"),
        name: r.get("name"),
        rel_path: r.get("rel_path"),
        remote_url: r.get("remote_url"),
        root_commit: r.get("root_commit"),
        source_kind: r.get("source_kind"),
        status: r.get("status"),
        status_source: r.get("status_source"),
        flags: r.get("flags"),
        languages: r.get("languages"),
        stack: r.get("stack"),
        head_commit: r.get("head_commit"),
        fingerprint: r.get("fingerprint"),
    }
}

fn map_card_row(r: sqlx::postgres::PgRow) -> anyhow::Result<CardRecord> {
    let body: serde_json::Value = r.get("body");
    let card: Card = serde_json::from_value(body)?;
    Ok(CardRecord {
        id: r.get("id"),
        project_id: r.get("project_id"),
        project_name: r.get("project_name"),
        fingerprint: r.get("fingerprint"),
        prompt_version: r.get("prompt_version"),
        model_id: r.get("model_id"),
        schema_version: r.get("schema_version"),
        card,
        tokens: r.get("tokens"),
        status: r.get("status"),
        lint_findings: r.get("lint_findings"),
    })
}
