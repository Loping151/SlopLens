use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Author {
    pub name: String,
    pub email: String,
}

impl Author {
    pub fn identity_key(&self) -> String {
        self.email.trim().to_ascii_lowercase()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Commit {
    pub oid: String,
    pub author: Author,
    pub author_time: i64,
    pub commit_time: i64,
    pub message: String,
    pub trailers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Add,
    Modify,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileChange {
    pub commit_oid: String,
    pub path: PathBuf,
    pub kind: FileChangeKind,
    pub lines_added: usize,
    pub lines_deleted: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hunk {
    pub path: PathBuf,
    pub commit_oid: String,
    pub start_line: usize,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Class,
    Method,
    Variable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub file: PathBuf,
    pub line: usize,
    pub is_exported: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    pub fn weight(self) -> f64 {
        match self {
            Self::High => 1.0,
            Self::Medium => 0.6,
            Self::Low => 0.3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Finding {
    pub rule_id: String,
    pub path: PathBuf,
    pub line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_name: Option<String>,
    pub severity: u8,
    pub confidence: Confidence,
    pub evidence: String,
    pub introduced_by: Option<String>,
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<FindingStatus>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    New,
    Persistent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attribution {
    pub author: Author,
    pub commit_oid: String,
    pub author_time: i64,
    pub commit_time: i64,
    pub ai_probability: f64,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Summary {
    pub commit_count: usize,
    pub author_count: usize,
    pub file_count: usize,
    pub finding_count: usize,
    pub total_debt_score: f64,
    pub ai_attributed_debt: f64,
    pub ai_attributed_ratio: f64,
    #[serde(default)]
    pub new_findings: usize,
    #[serde(default)]
    pub resolved_findings: usize,
    #[serde(default)]
    pub persistent_findings: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct BaselineComparison {
    pub previous_debt_score: f64,
    pub current_debt_score: f64,
    pub percent_change: f64,
    pub new_findings: usize,
    pub resolved_findings: usize,
    pub persistent_findings: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct RangeSummary {
    pub label: String,
    pub new_findings: usize,
    pub ai_suspect_findings: usize,
    pub debt_delta: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DebtTimelinePoint {
    pub commit_oid: String,
    pub commit_time: i64,
    pub cumulative_debt_score: f64,
    pub debt_delta: f64,
    pub ai_probability: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SlopReport {
    pub findings: Vec<Finding>,
    pub attributions: Vec<Attribution>,
    pub file_scores: BTreeMap<PathBuf, f64>,
    pub finding_scores: BTreeMap<String, f64>,
    pub debt_timeline: Vec<DebtTimelinePoint>,
    pub summary: Summary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_comparison: Option<BaselineComparison>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range_summary: Option<RangeSummary>,
}

pub fn stable_fingerprint(parts: &[&str]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for part in parts {
        for b in part.as_bytes() {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash ^= 0xff;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_key_lowercases_email() {
        let author = Author {
            name: "A".into(),
            email: "Dev@Example.COM".into(),
        };
        assert_eq!(author.identity_key(), "dev@example.com");
    }

    #[test]
    fn fingerprint_is_stable() {
        assert_eq!(
            stable_fingerprint(&["SL-001", "src/lib.rs", "12"]),
            stable_fingerprint(&["SL-001", "src/lib.rs", "12"])
        );
        assert_ne!(
            stable_fingerprint(&["SL-001", "src/lib.rs", "12"]),
            stable_fingerprint(&["SL-001", "src/lib.rs", "13"])
        );
    }
}
