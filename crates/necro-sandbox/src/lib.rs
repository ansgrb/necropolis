//! Capability-based file access confined to a registered project root (blueprint §15.2).
//!
//! All working-tree reads that can be triggered by an agent go through [`Sandbox`]. It holds a
//! `cap_std::fs::Dir` handle, so path resolution happens *beneath* that handle: `..` escapes,
//! absolute paths and symlinks pointing outside the root fail at the OS boundary (via
//! `openat2(RESOLVE_BENEATH)` on Linux) instead of relying on string checks alone. String
//! checks still run first, as defense in depth and to give clear error messages.

use cap_std::ambient_authority;
use cap_std::fs::Dir;
use globset::{Glob, GlobSet, GlobSetBuilder};
use std::io::Read;
use std::path::{Component, Path};

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("path rejected: {0}")]
    BadPath(String),
    #[error("path is on the denylist: {0}")]
    Denied(String),
    #[error("invalid line range {0}-{1}")]
    BadRange(u32, u32),
    #[error("file too large to serve ({0} bytes)")]
    TooLarge(u64),
    #[error("file is not valid UTF-8 text")]
    NotText,
    #[error("invalid denylist pattern {0}: {1}")]
    Glob(String, globset::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Glob denylist shared by the scanner (never index) and the sandbox (never serve).
#[derive(Clone)]
pub struct Denylist {
    set: GlobSet,
}

impl Denylist {
    pub fn new(patterns: &[String]) -> Result<Self, SandboxError> {
        let mut b = GlobSetBuilder::new();
        for p in patterns {
            b.add(Glob::new(p).map_err(|e| SandboxError::Glob(p.clone(), e))?);
        }
        Ok(Self { set: b.build().map_err(|e| SandboxError::Glob("<set>".into(), e))? })
    }

    pub fn is_denied(&self, rel: &str) -> bool {
        self.set.is_match(rel)
    }
}

/// Lexical validation of a root-relative path. Rejects anything that is not a plain,
/// forward-only relative path.
pub fn validate_rel_path(rel: &str) -> Result<(), SandboxError> {
    if rel.is_empty() || rel.len() > 4096 || rel.contains('\0') || rel.contains('\\') {
        return Err(SandboxError::BadPath(rel.into()));
    }
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err(SandboxError::BadPath(rel.into()));
    }
    for c in p.components() {
        match c {
            Component::Normal(_) => {}
            _ => return Err(SandboxError::BadPath(rel.into())),
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Snippet {
    pub start_line: u32,
    pub end_line: u32,
    pub total_lines: u32,
    pub text: String,
    /// True when the requested range was cut to `max_lines`.
    pub truncated: bool,
}

pub struct Sandbox {
    root: Dir,
    deny: Denylist,
    max_file_bytes: u64,
}

impl Sandbox {
    /// Opens a capability handle on `root`. This is the only ambient-authority call;
    /// everything after it is relative to the handle.
    pub fn open(root: &Path, deny: Denylist) -> Result<Self, SandboxError> {
        let root = Dir::open_ambient_dir(root, ambient_authority())?;
        Ok(Self { root, deny, max_file_bytes: 4 * 1024 * 1024 })
    }

    pub fn read_to_string(&self, rel: &str) -> Result<String, SandboxError> {
        validate_rel_path(rel)?;
        if self.deny.is_denied(rel) {
            return Err(SandboxError::Denied(rel.into()));
        }
        let file = self.root.open(rel)?; // resolution confined beneath the root handle
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(SandboxError::BadPath(rel.into()));
        }
        if meta.len() > self.max_file_bytes {
            return Err(SandboxError::TooLarge(meta.len()));
        }
        let mut buf = Vec::with_capacity(meta.len() as usize);
        file.into_std().take(self.max_file_bytes).read_to_end(&mut buf)?;
        String::from_utf8(buf).map_err(|_| SandboxError::NotText)
    }

    /// Reads an inclusive 1-based line range, capped at `max_lines`.
    pub fn read_lines(&self, rel: &str, start: u32, end: u32, max_lines: u32) -> Result<Snippet, SandboxError> {
        if start == 0 || end < start {
            return Err(SandboxError::BadRange(start, end));
        }
        let text = self.read_to_string(rel)?;
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len() as u32;
        if start > total.max(1) {
            return Err(SandboxError::BadRange(start, end));
        }
        let capped_end = end.min(start.saturating_add(max_lines.max(1)) - 1).min(total);
        let truncated = capped_end < end.min(total);
        let body = lines[(start - 1) as usize..capped_end as usize].join("\n");
        Ok(Snippet { start_line: start, end_line: capped_end, total_lines: total, text: body, truncated })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup() -> (std::path::PathBuf, Sandbox) {
        let base = std::env::temp_dir().join(format!("necro-sandbox-{}", ulid_like()));
        let root = base.join("root");
        fs::create_dir_all(root.join("src")).expect("mkdir");
        fs::write(root.join("src/main.rs"), "l1\nl2\nl3\nl4\nl5\n").expect("write");
        fs::write(root.join(".env"), "SECRET=1").expect("write");
        fs::write(base.join("outside.txt"), "top secret").expect("write");
        #[cfg(unix)]
        std::os::unix::fs::symlink(base.join("outside.txt"), root.join("src/escape.txt")).expect("symlink");
        let deny = Denylist::new(&necro_default_deny()).expect("deny");
        let sb = Sandbox::open(&root, deny).expect("open");
        (base, sb)
    }

    fn necro_default_deny() -> Vec<String> {
        vec!["**/.env".into(), "**/.ssh/**".into(), "**/*.pem".into()]
    }

    fn ulid_like() -> String {
        format!("{}-{:?}", std::process::id(), std::time::SystemTime::now())
            .replace([' ', ':', '{', '}', ','], "")
    }

    #[test]
    fn reads_range_and_caps() {
        let (_b, sb) = setup();
        let s = sb.read_lines("src/main.rs", 2, 4, 200).expect("read");
        assert_eq!(s.text, "l2\nl3\nl4");
        let s = sb.read_lines("src/main.rs", 1, 5, 2).expect("read");
        assert_eq!((s.end_line, s.truncated), (2, true));
    }

    #[test]
    fn rejects_traversal_and_absolute() {
        let (_b, sb) = setup();
        for bad in ["../outside.txt", "../../.ssh/id_rsa", "/etc/passwd", "src/../../outside.txt", "./src/main.rs", "src\\main.rs", ""] {
            assert!(sb.read_to_string(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn rejects_denylisted() {
        let (_b, sb) = setup();
        assert!(matches!(sb.read_to_string(".env"), Err(SandboxError::Denied(_))));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let (_b, sb) = setup();
        let r = sb.read_to_string("src/escape.txt");
        assert!(r.is_err(), "symlink escape must fail, got {r:?}");
    }

    #[test]
    fn bad_ranges() {
        let (_b, sb) = setup();
        assert!(sb.read_lines("src/main.rs", 0, 3, 10).is_err());
        assert!(sb.read_lines("src/main.rs", 4, 2, 10).is_err());
        assert!(sb.read_lines("src/main.rs", 99, 100, 10).is_err());
    }
}
