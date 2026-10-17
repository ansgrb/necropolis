use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_DATABASE_URL: &str = "postgres://necro:necro-dev-only@127.0.0.1:55432/necropolis";

/// `necro.toml`. Every field has a safe default so a missing file still works.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Overridden by `NECRO_DATABASE_URL`.
    pub database_url: String,
    /// Scan roots. `necro scan --all` walks these; MCP `scan` only accepts paths beneath them.
    pub roots: Vec<PathBuf>,
    pub scan: ScanConfig,
    pub mcp: McpConfig,
    pub llm: LlmConfig,
    /// Never returned by any tool, whether indexed or not (blueprint §15.2.3).
    pub denylist: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScanConfig {
    /// Files larger than this are recorded but not parsed.
    pub max_file_bytes: u64,
    /// Directory names never walked (vendored/generated code; blueprint §15.3.1).
    pub exclude_dirs: Vec<String>,
    /// Max nesting depth when looking for repos under a root.
    pub discover_depth: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpConfig {
    pub max_lines_per_pointer: u32,
    pub max_calls_per_session: u32,
    /// `scan` / `rescan_all` are disabled unless this is true.
    pub enable_mutating_tools: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    /// `none` (deterministic cards only) or `gemini`.
    pub provider: String,
    pub model: String,
    /// Name of the env var holding the key. The key itself never goes in the file.
    pub api_key_env: String,
    /// Token budget for the summarizer input bundle.
    pub bundle_token_budget: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            database_url: DEFAULT_DATABASE_URL.into(),
            roots: Vec::new(),
            scan: ScanConfig::default(),
            mcp: McpConfig::default(),
            llm: LlmConfig::default(),
            denylist: default_denylist(),
        }
    }
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            max_file_bytes: 512 * 1024,
            exclude_dirs: [
                "node_modules", "Pods", "vendor", "target", "build", "dist", ".dart_tool",
                ".gradle", ".idea", ".venv", "venv", "__pycache__", ".next", "DerivedData",
                "third_party", "generated", ".git",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            discover_depth: 4,
        }
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self { max_lines_per_pointer: 200, max_calls_per_session: 500, enable_mutating_tools: false }
    }
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: "none".into(),
            model: "gemini-2.5-flash".into(),
            api_key_env: "GEMINI_API_KEY".into(),
            bundle_token_budget: 6000,
        }
    }
}

pub fn default_denylist() -> Vec<String> {
    [
        "**/.env", "**/.env.*", "**/*.pem", "**/*.key", "**/id_rsa*", "**/id_ed25519*",
        "**/.ssh/**", "**/.aws/**", "**/.git/**", "**/*.keystore", "**/*.jks", "**/*.p12",
        "**/*.pfx", "**/google-services.json", "**/GoogleService-Info.plist", "**/.npmrc",
        "**/.netrc", "**/.pypirc", "**/credentials*", "**/*.kdbx", "**/secrets.*",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Config {
    /// Resolution order: explicit path, `NECRO_CONFIG`, `./necro.toml`,
    /// `~/.config/necropolis/necro.toml`, then defaults.
    pub fn load(explicit: Option<&Path>) -> anyhow::Result<(Self, Option<PathBuf>)> {
        let candidates: Vec<PathBuf> = match explicit {
            Some(p) => vec![p.to_path_buf()],
            None => {
                let mut v = Vec::new();
                if let Ok(p) = std::env::var("NECRO_CONFIG") {
                    v.push(PathBuf::from(p));
                }
                v.push(PathBuf::from("necro.toml"));
                if let Ok(home) = std::env::var("HOME") {
                    v.push(PathBuf::from(home).join(".config/necropolis/necro.toml"));
                }
                v
            }
        };
        let mut found = None;
        let mut cfg = Config::default();
        for c in candidates {
            if c.is_file() {
                let text = std::fs::read_to_string(&c)?;
                cfg = toml::from_str(&text).map_err(|e| anyhow::anyhow!("{}: {e}", c.display()))?;
                found = Some(c);
                break;
            } else if explicit.is_some() {
                anyhow::bail!("config file not found: {}", c.display());
            }
        }
        if let Ok(url) = std::env::var("NECRO_DATABASE_URL") {
            cfg.database_url = url;
        }
        cfg.roots = cfg.roots.iter().map(|r| expand_tilde(r)).collect();
        Ok((cfg, found))
    }
}

fn expand_tilde(p: &Path) -> PathBuf {
    match (p.strip_prefix("~"), std::env::var("HOME")) {
        (Ok(rest), Ok(home)) => PathBuf::from(home).join(rest),
        _ => p.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_config_uses_defaults() {
        let c: Config = toml::from_str("roots = [\"/tmp\"]\n[mcp]\nmax_lines_per_pointer = 50\n")
            .expect("parse");
        assert_eq!(c.mcp.max_lines_per_pointer, 50);
        assert!(!c.mcp.enable_mutating_tools);
        assert!(c.denylist.iter().any(|d| d.contains(".env")));
    }

    #[test]
    fn unknown_keys_rejected() {
        assert!(toml::from_str::<Config>("rootz = []").is_err());
    }
}
