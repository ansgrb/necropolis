//! The single redaction stage (blueprint §10, §15.2.4).
//!
//! Every sink — Postgres rows, the LLM bundle, embeddings, the search index and every MCP
//! response — consumes text that has passed through [`Redactor::redact`]. Sinks never run
//! their own checks, so there is exactly one place to get this right.
//!
//! Rules use the gitleaks.toml format. Matching is linear-time (`regex` has no backtracking),
//! and an Aho-Corasick keyword prefilter skips rules whose keywords don't occur in the input,
//! which keeps redact-on-egress cheap enough to run on every tool call.

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, MatchKind};
use regex::Regex;
use serde::Deserialize;
use std::collections::BTreeSet;

const DEFAULT_RULES: &str = include_str!("../rules/gitleaks-subset.toml");

#[derive(Debug, thiserror::Error)]
pub enum RedactError {
    #[error("invalid rules file: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("rule {id}: invalid regex: {source}")]
    Regex { id: String, source: regex::Error },
    #[error("keyword prefilter: {0}")]
    Prefilter(#[from] aho_corasick::BuildError),
}

#[derive(Debug, Deserialize)]
struct RulesFile {
    #[serde(default)]
    rules: Vec<RawRule>,
}

/// One `[[rules]]` entry. Unknown gitleaks fields (tags, allowlists, path) are ignored.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRule {
    id: String,
    regex: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
    entropy: Option<f64>,
    #[serde(default)]
    secret_group: usize,
}

struct Rule {
    id: String,
    re: Regex,
    entropy: Option<f64>,
    secret_group: usize,
    /// Indices into the prefilter's keyword list; empty = always run.
    keyword_ids: Vec<usize>,
}

/// A detected secret. Never contains the secret itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule_id: String,
    /// 1-based line number in the input.
    pub line: usize,
}

#[derive(Debug, Clone)]
pub struct Redacted {
    pub text: String,
    pub findings: Vec<Finding>,
}

pub struct Redactor {
    rules: Vec<Rule>,
    prefilter: Option<AhoCorasick>,
    literal_re: Regex,
}

impl Redactor {
    /// The embedded default rule set.
    pub fn new_default() -> Self {
        // The embedded rules are covered by tests, so failure here is a build defect.
        #[allow(clippy::expect_used)]
        Self::from_toml(DEFAULT_RULES).expect("embedded gitleaks rules must compile")
    }

    pub fn from_toml(text: &str) -> Result<Self, RedactError> {
        let file: RulesFile = toml::from_str(text)?;
        let mut keywords: Vec<String> = Vec::new();
        let mut rules = Vec::new();
        for raw in file.rules {
            let Some(pattern) = raw.regex else { continue }; // path-only rules don't apply to text
            let re = Regex::new(&pattern).map_err(|source| RedactError::Regex { id: raw.id.clone(), source })?;
            let keyword_ids = raw
                .keywords
                .iter()
                .map(|k| {
                    let k = k.to_lowercase();
                    keywords.iter().position(|e| *e == k).unwrap_or_else(|| {
                        keywords.push(k);
                        keywords.len() - 1
                    })
                })
                .collect();
            rules.push(Rule { id: raw.id, re, entropy: raw.entropy, secret_group: raw.secret_group, keyword_ids });
        }
        let prefilter = if keywords.is_empty() {
            None
        } else {
            Some(
                AhoCorasickBuilder::new()
                    .ascii_case_insensitive(true)
                    .match_kind(MatchKind::Standard)
                    .build(&keywords)?,
            )
        };
        // Quoted literals checked by the generic entropy pass.
        #[allow(clippy::expect_used)]
        let literal_re = Regex::new(r#"["'`]([A-Za-z0-9+/=_\-]{24,})["'`]"#).expect("static regex");
        Ok(Self { rules, prefilter, literal_re })
    }

    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Replaces every secret with `[REDACTED:<rule-id>]`. Idempotent.
    pub fn redact(&self, input: &str) -> Redacted {
        let present: BTreeSet<usize> = match &self.prefilter {
            Some(ac) => ac.find_overlapping_iter(input).map(|m| m.pattern().as_usize()).collect(),
            None => BTreeSet::new(),
        };

        // (start, end, rule_id) spans over the original input.
        let mut spans: Vec<(usize, usize, String)> = Vec::new();
        for rule in &self.rules {
            if !rule.keyword_ids.is_empty() && !rule.keyword_ids.iter().any(|k| present.contains(k)) {
                continue;
            }
            for caps in rule.re.captures_iter(input) {
                let Some(m) = caps.get(rule.secret_group).or_else(|| caps.get(0)) else { continue };
                let secret = m.as_str();
                if secret.starts_with("[REDACTED") {
                    continue;
                }
                if let Some(min) = rule.entropy {
                    // Entropy-gated (generic) rules also require a digit: real keys almost always
                    // have one, code identifiers like `config.api_key_value` usually don't.
                    if shannon_entropy(secret) < min || !secret.bytes().any(|b| b.is_ascii_digit()) {
                        continue;
                    }
                }
                spans.push((m.start(), m.end(), rule.id.clone()));
            }
        }

        // Generic high-entropy quoted literals (blueprint §10: "plus entropy checks").
        for caps in self.literal_re.captures_iter(input) {
            let Some(m) = caps.get(1) else { continue };
            let s = m.as_str();
            let has_digit = s.bytes().any(|b| b.is_ascii_digit());
            let has_upper = s.bytes().any(|b| b.is_ascii_uppercase());
            let has_lower = s.bytes().any(|b| b.is_ascii_lowercase());
            if has_digit && has_upper && has_lower && shannon_entropy(s) >= 4.5 {
                spans.push((m.start(), m.end(), "high-entropy-literal".into()));
            }
        }

        if spans.is_empty() {
            return Redacted { text: input.to_string(), findings: Vec::new() };
        }

        // Merge overlaps: keep the earliest-starting, widest span.
        spans.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        let mut merged: Vec<(usize, usize, String)> = Vec::new();
        for s in spans {
            match merged.last_mut() {
                Some(last) if s.0 < last.1 => last.1 = last.1.max(s.1),
                _ => merged.push(s),
            }
        }

        let mut out = String::with_capacity(input.len());
        let mut findings = Vec::with_capacity(merged.len());
        let mut cursor = 0;
        for (start, end, id) in merged {
            out.push_str(&input[cursor..start]);
            out.push_str("[REDACTED:");
            out.push_str(&id);
            out.push(']');
            findings.push(Finding { line: input[..start].matches('\n').count() + 1, rule_id: id });
            cursor = end;
        }
        out.push_str(&input[cursor..]);
        Redacted { text: out, findings }
    }
}

/// Shannon entropy in bits per character.
pub fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for b in s.bytes() {
        counts[b as usize] += 1;
    }
    let len = s.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / len;
            -p * p.log2()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fake, well-known example values (AWS docs) or randomly generated strings. None are live.
    const AWS_ID: &str = "AKIAIOSFODNN7EXAMPLE";
    const AWS_SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    fn r() -> Redactor {
        Redactor::new_default()
    }

    #[test]
    fn embedded_rules_compile() {
        assert!(r().rule_count() >= 15);
    }

    #[test]
    fn redacts_aws_pair() {
        let src = format!("aws_access_key_id = {AWS_ID}\naws_secret_access_key = \"{AWS_SECRET}\"\n");
        let out = r().redact(&src);
        assert!(!out.text.contains(AWS_ID), "{}", out.text);
        assert!(!out.text.contains(AWS_SECRET), "{}", out.text);
        assert!(out.findings.iter().any(|f| f.rule_id == "aws-access-token" && f.line == 1));
        assert!(out.findings.iter().any(|f| f.line == 2));
    }

    #[test]
    fn redacts_private_key_block() {
        let src = "x\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow\nabc\n-----END RSA PRIVATE KEY-----\ny";
        let out = r().redact(src);
        assert_eq!(out.text, "x\n[REDACTED:private-key]\ny");
    }

    #[test]
    fn redacts_connection_string_password_only() {
        let out = r().redact("DATABASE_URL=postgres://app:s3cr3tPassw0rd@db:5432/x");
        assert!(out.text.contains("postgres://app:[REDACTED:"), "{}", out.text);
        assert!(!out.text.contains("s3cr3tPassw0rd"));
    }

    #[test]
    fn redacts_tokens() {
        let gh = format!("ghp_{}", "aB3dE6gH9jK2mN5pQ8sT1vW4yZ7bC0eF3hJ6");
        let gcp = format!("AIza{}", "SyD3x4mpl3K3yV4lu3F0rT3st1ngPurp0s");
        let out = r().redact(&format!("token: {gh}\nconst k = \"{gcp}\";"));
        assert!(!out.text.contains(&gh) && !out.text.contains(&gcp), "{}", out.text);
    }

    #[test]
    fn leaves_ordinary_code_alone() {
        let src = "fn main() {\n    let api_key = config.api_key_value;\n    let token = parse_token(input);\n    println!(\"hello world\");\n}\n";
        let out = r().redact(src);
        assert_eq!(out.text, src);
        assert!(out.findings.is_empty());
    }

    #[test]
    fn generic_high_entropy_secret_assignment() {
        let out = r().redact("client_secret = \"q8Zr4Lw2Xv9Kp1Tn6Yb3Hm7Jd5Fs0Gc\"");
        assert!(!out.text.contains("q8Zr4Lw2Xv9Kp1Tn6Yb3Hm7Jd5Fs0Gc"), "{}", out.text);
    }

    #[test]
    fn idempotent() {
        let src = format!("key = {AWS_ID}");
        let once = r().redact(&src).text;
        let twice = r().redact(&once);
        assert_eq!(once, twice.text);
        assert!(twice.findings.is_empty());
    }

    #[test]
    fn entropy_math() {
        assert_eq!(shannon_entropy("aaaa"), 0.0);
        assert!((shannon_entropy("ab") - 1.0).abs() < 1e-9);
    }
}
