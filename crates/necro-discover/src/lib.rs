//! Repository discovery under configured scan roots (blueprint §8 Phase 1).

use ignore::WalkBuilder;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const MANIFEST_NAMES: &[&str] = &[
    "Cargo.toml",
    "pubspec.yaml",
    "package.json",
    "go.mod",
    "pyproject.toml",
    "Package.swift",
    "build.gradle",
    "build.gradle.kts",
];

#[derive(Debug, Clone)]
pub struct DiscoveredProject {
    pub root_path: PathBuf,
    pub project_path: PathBuf,
    pub rel_path: String,
    pub name: String,
    pub has_git: bool,
}

/// Discovers project directories under `root`.
/// A directory is considered a project if it contains `.git` or a top-level manifest.
/// Once a `.git` repo is found, we do not descend into its subdirectories looking for nested
/// projects unless `root` itself is a project.
pub fn discover_under_root(root: &Path, max_depth: usize, exclude_dirs: &[String]) -> anyhow::Result<Vec<DiscoveredProject>> {
    let canon_root = std::fs::canonicalize(root)?;
    let exclude: BTreeSet<String> = exclude_dirs.iter().cloned().collect();

    // If `root` itself is a repo or manifest project, return it directly.
    if is_project_dir(&canon_root) {
        let name = canon_root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "root".into());
        return Ok(vec![DiscoveredProject {
            has_git: canon_root.join(".git").exists(),
            root_path: canon_root.clone(),
            project_path: canon_root,
            rel_path: ".".into(),
            name,
        }]);
    }

    let mut found: Vec<DiscoveredProject> = Vec::new();
    let mut repo_prefixes: Vec<PathBuf> = Vec::new();

    let mut builder = WalkBuilder::new(&canon_root);
    builder.max_depth(Some(max_depth)).hidden(false).git_ignore(true).sort_by_file_name(|a, b| a.cmp(b));
    let exclude_clone = exclude.clone();
    builder.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true;
        }
        if entry.file_type().is_some_and(|ft| ft.is_dir()) {
            let name = entry.file_name().to_string_lossy();
            if name.starts_with('.') || exclude_clone.contains(name.as_ref()) {
                return false;
            }
        }
        true
    });

    for entry in builder.build().flatten() {
        if entry.depth() == 0 || !entry.file_type().is_some_and(|ft| ft.is_dir()) {
            continue;
        }
        let dir = entry.path();
        if repo_prefixes.iter().any(|p| dir.starts_with(p)) {
            continue;
        }
        if is_project_dir(dir) {
            let rel = dir
                .strip_prefix(&canon_root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|| ".".into());
            let name = dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| rel.clone());
            found.push(DiscoveredProject {
                root_path: canon_root.clone(),
                project_path: dir.to_path_buf(),
                rel_path: rel,
                name,
                has_git: dir.join(".git").exists(),
            });
            repo_prefixes.push(dir.to_path_buf());
        }
    }
    Ok(found)
}

pub fn is_project_dir(dir: &Path) -> bool {
    if dir.join(".git").exists() {
        return true;
    }
    MANIFEST_NAMES.iter().any(|m| dir.join(m).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_git_and_manifest_projects() {
        let tmp = std::env::temp_dir().join(format!("necro-disc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("alpha/.git")).unwrap();
        std::fs::create_dir_all(tmp.join("alpha/sub")).unwrap();
        std::fs::write(tmp.join("alpha/sub/Cargo.toml"), "[package]\nname=\"sub\"").unwrap();
        std::fs::create_dir_all(tmp.join("beta")).unwrap();
        std::fs::write(tmp.join("beta/pubspec.yaml"), "name: beta").unwrap();
        std::fs::create_dir_all(tmp.join("node_modules/pkg")).unwrap();
        std::fs::write(tmp.join("node_modules/pkg/package.json"), "{}").unwrap();

        let list = discover_under_root(&tmp, 3, &["node_modules".into()]).unwrap();
        let names: Vec<_> = list.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
