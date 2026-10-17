//! Hardened git interactions (blueprint §15.2.7).
//!
//! All git invocations go through [`Command::new("git")`] with fixed arguments, locked
//! configuration (`-c core.pager=cat -c core.fsmonitor= -c protocol.allow=never`),
//! `GIT_CONFIG_NOSYSTEM=1`, `GIT_TERMINAL_PROMPT=0`, and strict hex-SHA validation so
//! caller-supplied strings can never become flags (`--output=...`, `--exec=...`).

use std::path::{Component, Path};
use std::process::{Command, Stdio};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("invalid commit sha: {0}")]
    BadSha(String),
    #[error("invalid relative path: {0}")]
    BadPath(String),
    #[error("object {0} is not a commit (got {1})")]
    NotACommit(String, String),
    #[error("git command failed: {0}")]
    Command(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Default)]
pub struct GitFacts {
    pub is_repo: bool,
    pub is_dirty: bool,
    pub head_commit: Option<String>,
    pub root_commit: Option<String>,
    pub remote_url: Option<String>,
    pub last_commit_date: Option<String>,
    pub days_since_last_commit: Option<i64>,
    pub commit_count: u64,
}

/// Strictly validates a 40-char (SHA-1) or 64-char (SHA-256) hex object id.
pub fn validate_commit_sha(sha: &str) -> Result<(), GitError> {
    let ok_len = sha.len() == 40 || sha.len() == 64;
    if !ok_len || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(GitError::BadSha(sha.into()));
    }
    Ok(())
}

fn validate_rel_path(rel: &str) -> Result<(), GitError> {
    if rel.is_empty() || rel.len() > 4096 || rel.starts_with('-') || rel.contains('\0') || rel.contains('\\') || rel.contains(':') {
        return Err(GitError::BadPath(rel.into()));
    }
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(GitError::BadPath(rel.into()));
    }
    for c in p.components() {
        if !matches!(c, Component::Normal(_)) {
            return Err(GitError::BadPath(rel.into()));
        }
    }
    Ok(())
}

fn git_cmd(repo: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let out = Command::new("git")
        .current_dir(repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .args(["-c", "core.pager=cat", "-c", "core.fsmonitor=", "-c", "protocol.allow=never", "-c", "safe.directory=*"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    if !out.status.success() {
        return Err(GitError::Command(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(out.stdout)
}

/// Reads a blob at `<commit_sha>:<rel_path>` using `git cat-file`, which has no `--output` flag.
/// Enforces:
/// 1. `commit_sha` matches `^[0-9a-fA-F]{40|64}$`
/// 2. `git cat-file -t <commit_sha>` == `"commit"`
/// 3. `rel_path` is a normal relative path
/// 4. output is capped at `max_bytes`
pub fn read_blob_at_commit(repo: &Path, commit_sha: &str, rel_path: &str, max_bytes: usize) -> Result<String, GitError> {
    validate_commit_sha(commit_sha)?;
    validate_rel_path(rel_path)?;
    let kind_raw = git_cmd(repo, &["cat-file", "-t", commit_sha])?;
    let kind = String::from_utf8_lossy(&kind_raw).trim().to_string();
    if kind != "commit" {
        return Err(GitError::NotACommit(commit_sha.into(), kind));
    }
    let spec = format!("{commit_sha}:{rel_path}");
    let bytes = git_cmd(repo, &["cat-file", "blob", &spec])?;
    let slice = if bytes.len() > max_bytes { &bytes[..max_bytes] } else { &bytes[..] };
    String::from_utf8(slice.to_vec()).map_err(|_| GitError::Command("blob is not valid UTF-8".into()))
}

/// Fast check used for the clean-git short-circuit: returns `(head_sha, is_dirty)`.
pub fn quick_head_and_dirty(repo: &Path) -> Option<(String, bool)> {
    if !repo.join(".git").exists() {
        return None;
    }
    let head = String::from_utf8_lossy(&git_cmd(repo, &["rev-parse", "--verify", "HEAD"]).ok()?).trim().to_string();
    if validate_commit_sha(&head).is_err() {
        return None;
    }
    let status = git_cmd(repo, &["status", "--porcelain", "-uall"]).ok()?;
    let dirty = !status.iter().all(|b| b.is_ascii_whitespace());
    Some((head, dirty))
}

/// Collects repository facts for the scanner.
pub fn inspect_repo(repo: &Path) -> GitFacts {
    if !repo.join(".git").exists() {
        return GitFacts::default();
    }
    let head_commit = git_cmd(repo, &["rev-parse", "--verify", "HEAD"])
        .ok()
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .filter(|s| validate_commit_sha(s).is_ok());

    let is_dirty = git_cmd(repo, &["status", "--porcelain", "-uall"])
        .map(|b| !b.iter().all(|c| c.is_ascii_whitespace()))
        .unwrap_or(false);

    let root_commit = git_cmd(repo, &["rev-list", "--max-parents=0", "HEAD"])
        .ok()
        .and_then(|b| String::from_utf8_lossy(&b).lines().next().map(|s| s.trim().to_string()))
        .filter(|s| validate_commit_sha(s).is_ok());

    let remote_url = git_cmd(repo, &["config", "--get", "remote.origin.url"])
        .ok()
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .filter(|s| !s.is_empty());

    let (last_commit_date, days_since_last_commit) = git_cmd(repo, &["log", "-1", "--format=%cs %ct"])
        .ok()
        .and_then(|b| {
            let s = String::from_utf8_lossy(&b);
            let mut parts = s.split_whitespace();
            let date = parts.next()?.to_string();
            let ts: i64 = parts.next()?.parse().ok()?;
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
            Some((Some(date), Some(((now - ts).max(0)) / 86_400)))
        })
        .unwrap_or((None, None));

    let commit_count = git_cmd(repo, &["rev-list", "--count", "HEAD"])
        .ok()
        .and_then(|b| String::from_utf8_lossy(&b).trim().parse::<u64>().ok())
        .unwrap_or(0);

    GitFacts {
        is_repo: true,
        is_dirty,
        head_commit,
        root_commit,
        remote_url,
        last_commit_date,
        days_since_last_commit,
        commit_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_argument_injection_in_sha() {
        for bad in [
            "--output=/tmp/pwned",
            "-c core.pager=sh",
            "HEAD",
            "main",
            "deadbeef",
            "0123456789abcdef0123456789abcdef0123456g",
            "0123456789abcdef0123456789abcdef01234567; rm -rf /",
        ] {
            assert!(validate_commit_sha(bad).is_err(), "must reject {bad:?}");
        }
        assert!(validate_commit_sha("0123456789abcdef0123456789abcdef01234567").is_ok());
    }

    #[test]
    fn rejects_bad_paths_in_blob_read() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        for bad in ["../secret", "/etc/passwd", "--help", "a:b", "a\\b", ""] {
            let r = read_blob_at_commit(Path::new("."), sha, bad, 1024);
            assert!(matches!(r, Err(GitError::BadPath(_))), "must reject {bad:?}: {r:?}");
        }
    }
}
