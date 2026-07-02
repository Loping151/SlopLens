use crate::git_ingest::GitHistory;
use crate::ir::{Attribution, DebtTimelinePoint, Finding, SlopReport, Summary};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

pub fn score(
    history: &GitHistory,
    findings: Vec<Finding>,
    attributions: Vec<Attribution>,
) -> SlopReport {
    let commit_index: HashMap<&str, usize> = history
        .commits
        .iter()
        .enumerate()
        .map(|(idx, commit)| (commit.oid.as_str(), idx))
        .collect();
    let attr_by_commit: HashMap<&str, f64> = attributions
        .iter()
        .map(|attr| (attr.commit_oid.as_str(), attr.ai_probability))
        .collect();
    let total_commits = history.commits.len().max(1);
    let mut file_scores: BTreeMap<PathBuf, f64> = BTreeMap::new();
    let mut finding_scores: BTreeMap<String, f64> = BTreeMap::new();
    let mut debt_by_commit: HashMap<String, f64> = HashMap::new();
    let mut total_debt = 0.0;
    let mut ai_debt = 0.0;

    for finding in &findings {
        let persistence = finding
            .introduced_by
            .as_deref()
            .and_then(|oid| commit_index.get(oid).copied())
            .map(|idx| (total_commits - idx) as f64 / total_commits as f64)
            .unwrap_or(1.0);
        let delta = 1.0;
        let debt = f64::from(finding.severity) * finding.confidence.weight() * persistence * delta;
        *file_scores.entry(finding.path.clone()).or_insert(0.0) += debt;
        finding_scores.insert(finding.fingerprint.clone(), round2(debt));
        if let Some(oid) = &finding.introduced_by {
            *debt_by_commit.entry(oid.clone()).or_insert(0.0) += debt;
        }
        total_debt += debt;
        let p_ai = finding
            .introduced_by
            .as_deref()
            .and_then(|oid| attr_by_commit.get(oid).copied())
            .unwrap_or(0.0);
        ai_debt += debt * p_ai;
    }

    let author_count = history
        .commits
        .iter()
        .map(|c| c.author.identity_key())
        .collect::<HashSet<_>>()
        .len();
    let file_count = history
        .changes
        .iter()
        .map(|c| c.path.clone())
        .collect::<HashSet<_>>()
        .len();
    let summary = Summary {
        commit_count: history.commits.len(),
        author_count,
        file_count,
        finding_count: findings.len(),
        total_debt_score: round2(total_debt),
        ai_attributed_debt: round2(ai_debt),
        ai_attributed_ratio: if total_debt > 0.0 {
            round2(ai_debt / total_debt)
        } else {
            0.0
        },
        ..Summary::default()
    };
    for score in file_scores.values_mut() {
        *score = round2(*score);
    }
    let debt_timeline = debt_timeline(history, &debt_by_commit, &attr_by_commit);
    SlopReport {
        findings,
        attributions,
        file_scores,
        finding_scores,
        debt_timeline,
        summary,
        baseline_comparison: None,
        range_summary: None,
    }
}

pub fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn debt_timeline(
    history: &GitHistory,
    debt_by_commit: &HashMap<String, f64>,
    attr_by_commit: &HashMap<&str, f64>,
) -> Vec<DebtTimelinePoint> {
    let mut cumulative = 0.0;
    let mut points = Vec::new();
    let mut commits = history.commits.iter().collect::<Vec<_>>();
    commits.sort_by_key(|commit| commit.commit_time);
    for commit in commits {
        let delta = debt_by_commit.get(&commit.oid).copied().unwrap_or(0.0);
        cumulative += delta;
        let ai_probability = attr_by_commit
            .get(commit.oid.as_str())
            .copied()
            .unwrap_or(0.0);
        if delta > 0.0 || ai_probability >= 0.3 {
            points.push(DebtTimelinePoint {
                commit_oid: commit.oid.clone(),
                commit_time: commit.commit_time,
                cumulative_debt_score: round2(cumulative),
                debt_delta: round2(delta),
                ai_probability,
            });
        }
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Author, Commit, Confidence, Finding};

    #[test]
    fn debt_score_uses_confidence_persistence_and_ai_slice() {
        let author = Author {
            name: "A".into(),
            email: "a@example.com".into(),
        };
        let history = GitHistory {
            commits: vec![
                Commit {
                    oid: "a".into(),
                    author: author.clone(),
                    author_time: 1,
                    commit_time: 1,
                    message: "a".into(),
                    trailers: vec![],
                },
                Commit {
                    oid: "b".into(),
                    author: author.clone(),
                    author_time: 2,
                    commit_time: 2,
                    message: "b".into(),
                    trailers: vec![],
                },
            ],
            changes: vec![],
            hunks: vec![],
        };
        let finding = Finding {
            rule_id: "SL-001".into(),
            path: PathBuf::from("src/lib.rs"),
            line: 1,
            symbol_name: Some("dead".into()),
            severity: 4,
            confidence: Confidence::Medium,
            evidence: "x".into(),
            introduced_by: Some("b".into()),
            fingerprint: "fp".into(),
            status: None,
        };
        let attrs = vec![Attribution {
            author,
            commit_oid: "b".into(),
            author_time: 2,
            commit_time: 2,
            ai_probability: 0.5,
            evidence: vec!["medium".into()],
        }];
        let report = score(&history, vec![finding], attrs);
        assert_eq!(report.summary.total_debt_score, 1.2);
        assert_eq!(report.summary.ai_attributed_debt, 0.6);
        assert_eq!(report.finding_scores["fp"], 1.2);
        assert_eq!(report.debt_timeline[0].cumulative_debt_score, 1.2);
    }
}
