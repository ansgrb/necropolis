use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! str_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];
            pub fn as_str(&self) -> &'static str {
                match self { $($name::$variant => $s),+ }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { f.write_str(self.as_str()) }
        }

        impl std::str::FromStr for $name {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s { $($s => Ok($name::$variant),)+ other => Err(format!("invalid {}: {other}", stringify!($name))) }
            }
        }
    };
}

str_enum!(
    /// How the project's source was obtained. Mirrors `project.source_kind`.
    SourceKind { GitClean => "git_clean", GitDirty => "git_dirty", PlainDir => "plain_dir", Synthetic => "synthetic" }
);

str_enum!(
    /// Lifecycle status. Mirrors `project.status`.
    ProjectStatus {
        Unknown => "unknown", Active => "active", Finished => "finished",
        Abandoned => "abandoned", Archived => "archived", Recovered => "recovered",
    }
);

str_enum!(
    /// Per-project user flags. Mirrors the `project.flags` CHECK constraint.
    ProjectFlag { Private => "private", NoLlm => "no_llm", Skip => "skip" }
);

str_enum!(
    /// Symbol kinds. Mirrors `symbol.kind`.
    SymbolKind {
        Class => "class", Fn => "fn", Method => "method", Const => "const",
        Type => "type", Field => "field", Enum => "enum", Interface => "interface",
    }
);

str_enum!(
    /// Per-card review state. Mirrors `card.status`.
    CardStatus { Quarantined => "quarantined", Approved => "approved", Rejected => "rejected" }
);

/// Heuristic status from git activity. Always `status_source = deterministic`;
/// a human override wins and is never replaced by a scan.
pub fn infer_status(days_since_last_commit: Option<i64>) -> ProjectStatus {
    match days_since_last_commit {
        None => ProjectStatus::Unknown,
        Some(d) if d <= 90 => ProjectStatus::Active,
        Some(_) => ProjectStatus::Abandoned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_round_trip() {
        for k in SymbolKind::ALL {
            assert_eq!(k.as_str().parse::<SymbolKind>().ok(), Some(*k));
        }
        assert!("root".parse::<ProjectFlag>().is_err());
    }

    #[test]
    fn status_heuristic() {
        assert_eq!(infer_status(None), ProjectStatus::Unknown);
        assert_eq!(infer_status(Some(3)), ProjectStatus::Active);
        assert_eq!(infer_status(Some(400)), ProjectStatus::Abandoned);
    }
}
