use serde::{Deserialize, Serialize};

/// Bump when the card shape changes. Part of the card cache key (blueprint §15.4).
pub const CARD_SCHEMA_VERSION: i32 = 1;
/// Bump when the summarizer prompt changes. Part of the card cache key.
pub const PROMPT_VERSION: i32 = 1;

/// The L0 card: the ≤ 600-token summary an agent reads first.
///
/// Field caps are enforced twice: by `schemas/card.schema.json` (validated in
/// `necro-summarize`) and by the deterministic lint. Keep the two in sync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Card {
    pub name: String,
    /// One sentence, ≤ 200 chars.
    pub one_liner: String,
    pub status: String,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub stack: Vec<String>,
    #[serde(default)]
    pub entry_points: Vec<String>,
    /// The reusable value of the project.
    #[serde(default)]
    pub ideas: Vec<CardIdea>,
    #[serde(default)]
    pub reusable_modules: Vec<String>,
    #[serde(default)]
    pub gotchas: Vec<String>,
    /// Typed, denylisted, rendered only as inert code (blueprint §15.3.4).
    #[serde(default)]
    pub commands: Vec<String>,
    /// Human-only field. The summarizer must never fill it (blueprint review S5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why_stopped: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_commit_date: Option<String>,
    pub source_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    /// `deterministic` or `llm:<model-id>`.
    pub generated_by: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardIdea {
    pub name: String,
    /// Relative path with optional line range, e.g. `lib/sync/queue.dart:12-210`.
    #[serde(rename = "where")]
    pub location: String,
}

impl Card {
    /// Compact Markdown rendering for agents. Commands are fenced so they are never prose.
    pub fn to_markdown(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("# {} ({})\n{}\n", self.name, self.status, self.one_liner));
        let list = |label: &str, v: &[String], s: &mut String| {
            if !v.is_empty() {
                s.push_str(&format!("- {label}: {}\n", v.join(", ")));
            }
        };
        list("languages", &self.languages, &mut s);
        list("stack", &self.stack, &mut s);
        list("entry points", &self.entry_points, &mut s);
        list("reusable modules", &self.reusable_modules, &mut s);
        if let Some(d) = &self.last_commit_date {
            s.push_str(&format!("- last commit: {d}\n"));
        }
        if !self.ideas.is_empty() {
            s.push_str("## ideas\n");
            for i in &self.ideas {
                s.push_str(&format!("- {} — `{}`\n", i.name, i.location));
            }
        }
        if !self.gotchas.is_empty() {
            s.push_str("## gotchas\n");
            for g in &self.gotchas {
                s.push_str(&format!("- {g}\n"));
            }
        }
        if !self.commands.is_empty() {
            s.push_str("## commands\n```text\n");
            for c in &self.commands {
                s.push_str(c);
                s.push('\n');
            }
            s.push_str("```\n");
        }
        if let Some(w) = &self.why_stopped {
            s.push_str(&format!("- why stopped (human): {w}\n"));
        }
        s
    }
}
