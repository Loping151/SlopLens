use crate::ir::{BaselineComparison, FindingStatus, SlopReport};
use crate::scoring::round2;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const BASELINE_VERSION: u8 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Baseline {
    pub version: u8,
    pub total_debt_score: f64,
    pub findings: Vec<BaselineFinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BaselineFinding {
    pub fingerprint: String,
    pub debt_score: f64,
}

impl Baseline {
    pub fn from_report(report: &SlopReport) -> Self {
        let findings = report
            .findings
            .iter()
            .map(|finding| BaselineFinding {
                fingerprint: finding.fingerprint.clone(),
                debt_score: report
                    .finding_scores
                    .get(&finding.fingerprint)
                    .copied()
                    .unwrap_or(0.0),
            })
            .collect();
        Self {
            version: BASELINE_VERSION,
            total_debt_score: report.summary.total_debt_score,
            findings,
        }
    }

    fn score_by_fingerprint(&self) -> BTreeMap<String, f64> {
        self.findings
            .iter()
            .map(|finding| (finding.fingerprint.clone(), finding.debt_score))
            .collect()
    }
}

pub fn baseline_path(repo: &Path) -> PathBuf {
    repo.join(".slop").join("baseline.json")
}

pub fn load_if_exists(repo: &Path) -> Result<Option<Baseline>> {
    let path = baseline_path(repo);
    if !path.exists() {
        return Ok(None);
    }
    let raw =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let baseline = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    Ok(Some(baseline))
}

pub fn save(repo: &Path, report: &SlopReport) -> Result<PathBuf> {
    let path = baseline_path(repo);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let baseline = Baseline::from_report(report);
    let raw = serde_json::to_string_pretty(&baseline)?;
    fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

pub fn apply(report: &mut SlopReport, baseline: &Baseline) {
    let baseline_scores = baseline.score_by_fingerprint();
    let baseline_fps = baseline_scores.keys().cloned().collect::<HashSet<_>>();
    let current_fps = report
        .findings
        .iter()
        .map(|finding| finding.fingerprint.clone())
        .collect::<HashSet<_>>();

    let new_findings = current_fps.difference(&baseline_fps).count();
    let resolved_findings = baseline_fps.difference(&current_fps).count();
    let persistent_findings = current_fps.intersection(&baseline_fps).count();

    for finding in &mut report.findings {
        finding.status = Some(if baseline_fps.contains(&finding.fingerprint) {
            FindingStatus::Persistent
        } else {
            FindingStatus::New
        });
    }

    let current_debt_score = report.summary.total_debt_score;
    let percent_change = if baseline.total_debt_score > 0.0 {
        round2((current_debt_score - baseline.total_debt_score) / baseline.total_debt_score * 100.0)
    } else if current_debt_score > 0.0 {
        100.0
    } else {
        0.0
    };

    report.summary.new_findings = new_findings;
    report.summary.resolved_findings = resolved_findings;
    report.summary.persistent_findings = persistent_findings;
    report.baseline_comparison = Some(BaselineComparison {
        previous_debt_score: baseline.total_debt_score,
        current_debt_score,
        percent_change,
        new_findings,
        resolved_findings,
        persistent_findings,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Confidence, Finding, Summary};

    fn finding(fp: &str) -> Finding {
        Finding {
            rule_id: "SL-001".into(),
            path: PathBuf::from("src/lib.rs"),
            line: 1,
            symbol_name: Some(fp.into()),
            severity: 3,
            confidence: Confidence::High,
            evidence: "unused".into(),
            introduced_by: None,
            fingerprint: fp.into(),
            status: None,
        }
    }

    #[test]
    fn baseline_apply_counts_new_resolved_and_persistent() {
        let baseline = Baseline {
            version: 1,
            total_debt_score: 10.0,
            findings: vec![
                BaselineFinding {
                    fingerprint: "old".into(),
                    debt_score: 4.0,
                },
                BaselineFinding {
                    fingerprint: "same".into(),
                    debt_score: 6.0,
                },
            ],
        };
        let mut report = SlopReport {
            findings: vec![finding("same"), finding("new")],
            summary: Summary {
                total_debt_score: 12.0,
                finding_count: 2,
                ..Summary::default()
            },
            ..SlopReport::default()
        };

        apply(&mut report, &baseline);

        assert_eq!(report.summary.new_findings, 1);
        assert_eq!(report.summary.resolved_findings, 1);
        assert_eq!(report.summary.persistent_findings, 1);
        assert_eq!(report.baseline_comparison.unwrap().percent_change, 20.0);
        assert_eq!(report.findings[1].status, Some(FindingStatus::New));
    }
}
