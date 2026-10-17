//! Incremental scanner orchestrator, BLAKE3 Merkle fingerprinting, and stat-cache short-circuit
//! (blueprint §8 Phase 1 & §15.4).

use ignore::WalkBuilder;
use necro_core::{Config, SourceKind, infer_status};
use necro_discover::DiscoveredProject;
use necro_git::{inspect_repo, quick_head_and_dirty};
use necro_parse::{parse_manifest_stack, parse_source};
use necro_redact::Redactor;
use necro_sandbox::Denylist;
use necro_store::{FileUpsert, Store, SymbolInsert};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::time::Instant;

const SCANNER_CONFIG_VERSION: &str = "necro-scanner-v1";

#[derive(Debug, Clone)]
pub struct ScanReport {
    pub project_id: String,
    pub project_name: String,
    pub fingerprint: String,
    pub files_total: usize,
    pub files_reparsed: usize,
    pub symbols_extracted: usize,
    pub secrets_redacted: i32,
    pub skipped_unchanged: bool,
    pub elapsed_ms: u128,
}

/// Scans one discovered project into Postgres.
///
/// Short-circuits in two ways to meet the `< 2 s` no-op rescan exit criterion (§15.4):
/// 1. **Clean git short-circuit:** if `HEAD` and `git status --porcelain` match a previous
///    `git_clean` scan, zero files are walked or read.
/// 2. **Per-file stat cache:** for every walked file whose `(mtime_ns, size_bytes, inode)`
///    matches the stored row in `file`, we reuse its `content_hash` without reading or
///    re-parsing the file. If the resulting Merkle fingerprint equals the project's stored
///    fingerprint, no DB writes happen.
pub async fn scan_project(
    store: &Store,
    cfg: &Config,
    redactor: &Redactor,
    denylist: &Denylist,
    disc: &DiscoveredProject,
) -> anyhow::Result<ScanReport> {
    let t0 = Instant::now();
    let root_str = disc.root_path.to_string_lossy().into_owned();
    let root_id = store.ensure_root(&root_str).await?;
    let existing = store.get_project_by_root_rel(&root_id, &disc.rel_path).await?;

    // 1. Clean-git fast path before walking the tree.
    if let (Some(prev), Some((head, false))) = (&existing, quick_head_and_dirty(&disc.project_path)) {
        if prev.source_kind == "git_clean"
            && prev.head_commit.as_deref() == Some(head.as_str())
            && prev.fingerprint.is_some()
        {
            let fp = prev.fingerprint.clone().unwrap_or_default();
            return Ok(ScanReport {
                project_id: prev.id.clone(),
                project_name: prev.name.clone(),
                fingerprint: fp,
                files_total: 0,
                files_reparsed: 0,
                symbols_extracted: 0,
                secrets_redacted: 0,
                skipped_unchanged: true,
                elapsed_ms: t0.elapsed().as_millis(),
            });
        }
    }

    let git_facts = inspect_repo(&disc.project_path);
    let source_kind = if git_facts.is_repo {
        if git_facts.is_dirty { SourceKind::GitDirty } else { SourceKind::GitClean }
    } else {
        SourceKind::PlainDir
    };
    let inferred_status = infer_status(git_facts.days_since_last_commit);

    let stat_cache = match &existing {
        Some(p) => store.load_file_stats(&p.id).await?,
        None => Default::default(),
    };

    let walked = walk_project_files(&disc.project_path, &cfg.scan.exclude_dirs, denylist)?;
    let mut hash_entries: BTreeMap<String, String> = BTreeMap::new();
    let mut changed_files: Vec<FileUpsert> = Vec::new();
    let mut keep_paths: Vec<String> = Vec::with_capacity(walked.len());
    let mut languages: BTreeSet<String> = BTreeSet::new();
    let mut stack: BTreeSet<String> = BTreeSet::new();
    let mut total_secrets: i32 = 0;
    let mut total_symbols: usize = 0;

    for rel in &walked {
        let full = disc.project_path.join(rel);
        let Ok(meta) = fs::symlink_metadata(&full) else { continue };
        if !meta.is_file() || meta.len() > cfg.scan.max_file_bytes {
            continue;
        }
        let (mtime_ns, size_bytes, inode) = stat_tuple(&meta);
        keep_paths.push(rel.clone());

        // Check stat cache first.
        if let Some(cached) = stat_cache.get(rel) {
            if cached.mtime_ns == mtime_ns && cached.size_bytes == size_bytes && cached.inode == inode {
                hash_entries.insert(rel.clone(), cached.content_hash.clone());
                if let Some(ref l) = cached.lang {
                    languages.insert(l.clone());
                }
                if is_manifest_name(rel) {
                    if let Ok(raw) = fs::read_to_string(&full) {
                        for tag in parse_manifest_stack(rel, &raw) {
                            stack.insert(tag);
                        }
                    }
                }
                continue;
            }
        }

        // Read & redact before anything else touches the content (§10).
        let Ok(raw_bytes) = fs::read(&full) else { continue };
        let Ok(raw_text) = String::from_utf8(raw_bytes) else { continue }; // skip binary files
        let content_hash = blake3::hash(raw_text.as_bytes()).to_hex().to_string();
        hash_entries.insert(rel.clone(), content_hash.clone());

        if is_manifest_name(rel) {
            for tag in parse_manifest_stack(rel, &raw_text) {
                stack.insert(tag);
            }
        }

        // If the content hash matches despite a stat bump (e.g., `touch`), reuse the parsed symbols.
        if let Some(cached) = stat_cache.get(rel) {
            if cached.content_hash == content_hash {
                if let Some(ref l) = cached.lang {
                    languages.insert(l.clone());
                }
                continue;
            }
        }

        let redacted = redactor.redact(&raw_text);
        let redaction_count = redacted.findings.len() as i32;
        total_secrets += redaction_count;

        let parsed = parse_source(rel, &redacted.text);
        if let Some(ref l) = parsed.lang {
            languages.insert(l.clone());
        }
        total_symbols += parsed.symbols.len();

        let symbols = parsed
            .symbols
            .into_iter()
            .map(|s| SymbolInsert {
                name: s.name,
                kind: s.kind,
                start_line: s.start_line,
                end_line: s.end_line,
                signature: s.signature,
            })
            .collect();

        changed_files.push(FileUpsert {
            path: rel.clone(),
            lang: parsed.lang,
            role: parsed.role,
            content_hash,
            mtime_ns,
            size_bytes,
            inode,
            loc: parsed.loc,
            redactions: redaction_count,
            symbols,
            imports: parsed.imports,
        });
    }

    let fingerprint = compute_fingerprint(&hash_entries);

    // If the Merkle fingerprint is identical to the stored one and no files were added/removed,
    // skip all DB writes.
    if let Some(prev) = &existing {
        if prev.fingerprint.as_deref() == Some(fingerprint.as_str())
            && changed_files.is_empty()
            && keep_paths.len() == stat_cache.len()
        {
            return Ok(ScanReport {
                project_id: prev.id.clone(),
                project_name: prev.name.clone(),
                fingerprint,
                files_total: keep_paths.len(),
                files_reparsed: 0,
                symbols_extracted: 0,
                secrets_redacted: 0,
                skipped_unchanged: true,
                elapsed_ms: t0.elapsed().as_millis(),
            });
        }
    }

    let lang_vec: Vec<String> = languages.into_iter().collect();
    let stack_vec: Vec<String> = stack.into_iter().collect();

    let project_id = store
        .upsert_project(
            &root_id,
            &disc.name,
            &disc.rel_path,
            git_facts.remote_url.as_deref(),
            git_facts.root_commit.as_deref(),
            source_kind,
            inferred_status,
            &lang_vec,
            &stack_vec,
            git_facts.head_commit.as_deref(),
            &fingerprint,
        )
        .await?;

    let reparsed = changed_files.len();
    store.apply_file_changes(&project_id, &changed_files, &keep_paths).await?;
    store
        .record_scan_run(&project_id, &fingerprint, git_facts.head_commit.as_deref(), total_secrets, "ok")
        .await?;

    Ok(ScanReport {
        project_id,
        project_name: disc.name.clone(),
        fingerprint,
        files_total: keep_paths.len(),
        files_reparsed: reparsed,
        symbols_extracted: total_symbols,
        secrets_redacted: total_secrets,
        skipped_unchanged: false,
        elapsed_ms: t0.elapsed().as_millis(),
    })
}

/// Computes the deterministic Merkle fingerprint over sorted `(relpath, content_hash)` pairs.
pub fn compute_fingerprint(files: &BTreeMap<String, String>) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(SCANNER_CONFIG_VERSION.as_bytes());
    hasher.update(b"\n");
    for (path, hash) in files {
        hasher.update(path.as_bytes());
        hasher.update(b"\0");
        hasher.update(hash.as_bytes());
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex().to_string()
}

fn walk_project_files(project_dir: &Path, exclude_dirs: &[String], denylist: &Denylist) -> anyhow::Result<Vec<String>> {
    let exclude: BTreeSet<String> = exclude_dirs.iter().cloned().collect();
    let mut b = WalkBuilder::new(project_dir);
    b.hidden(false).git_ignore(true).git_global(false).git_exclude(true).sort_by_file_name(|x, y| x.cmp(y));
    b.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true;
        }
        let name = entry.file_name().to_string_lossy();
        if entry.file_type().is_some_and(|ft| ft.is_dir()) {
            return !exclude.contains(name.as_ref()) && name != ".git";
        }
        true
    });

    let mut out = Vec::new();
    for entry in b.build().flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(project_dir) else { continue };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str.is_empty() || denylist.is_denied(&rel_str) {
            continue;
        }
        out.push(rel_str);
    }
    Ok(out)
}

fn is_manifest_name(rel: &str) -> bool {
    let name = Path::new(rel).file_name().and_then(|s| s.to_str()).unwrap_or("");
    matches!(
        name,
        "Cargo.toml"
            | "pubspec.yaml"
            | "package.json"
            | "go.mod"
            | "pyproject.toml"
            | "Package.swift"
            | "build.gradle"
            | "build.gradle.kts"
    )
}

fn stat_tuple(meta: &fs::Metadata) -> (Option<i64>, Option<i64>, Option<i64>) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (Some(meta.mtime_nsec()), Some(meta.len() as i64), Some(meta.ino() as i64))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        (None, Some(meta.len() as i64), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_deterministic_and_sensitive() {
        let mut a = BTreeMap::new();
        a.insert("src/main.rs".into(), "h1".into());
        a.insert("Cargo.toml".into(), "h2".into());
        let fp1 = compute_fingerprint(&a);
        let fp2 = compute_fingerprint(&a);
        assert_eq!(fp1, fp2);
        a.insert("src/main.rs".into(), "h1_changed".into());
        assert_ne!(fp1, compute_fingerprint(&a));
    }
}
