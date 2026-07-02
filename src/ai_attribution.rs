use crate::git_ingest::GitHistory;
use crate::ir::Attribution;
use std::collections::HashMap;

pub fn attribute(history: &GitHistory) -> Vec<Attribution> {
    let mut by_commit: HashMap<&str, CommitStats> = HashMap::new();
    for change in &history.changes {
        let stats = by_commit.entry(&change.commit_oid).or_default();
        stats.lines_added += change.lines_added;
        stats.lines_deleted += change.lines_deleted;
        stats.files += 1;
        let path = change.path.to_string_lossy().to_ascii_lowercase();
        if path.contains("test")
            || path.contains("spec")
            || path.contains("doc")
            || path.ends_with(".md")
        {
            stats.test_doc_files += 1;
        }
    }

    let mut author_history: HashMap<String, Vec<CommitProfile>> = HashMap::new();
    let mut results = Vec::new();
    for commit in &history.commits {
        let stats = by_commit
            .get(commit.oid.as_str())
            .cloned()
            .unwrap_or_default();
        let mut evidence = Vec::new();
        let mut score = 0.0f64;

        for trailer in &commit.trailers {
            let lower = trailer.to_ascii_lowercase();
            if lower.contains("copilot")
                || lower.contains("bot")
                || lower.contains("ai")
                || lower.contains("github.dev")
                || lower.contains("users.noreply.github.dev")
            {
                evidence.push(format!("trailer:{trailer}(+0.9)"));
                score += 0.9;
            }
        }
        let lower_message = commit.message.to_ascii_lowercase();
        if lower_message.contains("copilot")
            || lower_message.contains("generated-by: ai")
            || lower_message.contains("ai-assisted")
            || commit
                .author
                .email
                .to_ascii_lowercase()
                .contains("users.noreply.github.dev")
        {
            evidence.push("metadata:AI or GitHub web authoring(+0.9)".into());
            score += 0.9;
        }

        let churn = stats.lines_added + stats.lines_deleted;
        if churn > 200 && stats.files <= 2 {
            evidence.push(format!(
                "churn:large low-coordination diff ({churn} changed lines across {} files)(+0.5)",
                stats.files
            ));
            score += 0.5;
        }
        if is_large_multifile_burst_commit(
            history,
            &commit.oid,
            commit.commit_time,
            &commit.author.identity_key(),
            &stats,
        ) {
            evidence.push(
                "burst:large multi-file commit landed within 60 seconds of same-author work(+0.1)"
                    .into(),
            );
            score += 0.1;
        }
        if stats.files >= 3 && stats.test_doc_files * 100 / stats.files.max(1) >= 70 {
            evidence.push("shape:unusually high test/documentation file ratio(+0.5)".into());
            score += 0.5;
        }
        let key = commit.author.identity_key();
        let profiles = author_history.entry(key).or_default();
        if profiles.len() >= 2 {
            let avg_churn =
                profiles.iter().map(|p| p.churn as f64).sum::<f64>() / profiles.len() as f64;
            let avg_msg =
                profiles.iter().map(|p| p.message_len as f64).sum::<f64>() / profiles.len() as f64;
            if (churn as f64) > (avg_churn * 3.0).max(120.0)
                || (commit.message.len() as f64) > (avg_msg * 3.0).max(80.0)
            {
                evidence.push(
                    "style:author style/churn changed sharply versus prior commits(+0.5)".into(),
                );
                score += 0.5;
            }
        }

        if weak_template_message(&lower_message) {
            let subject = lower_message.lines().next().unwrap_or_default().trim();
            evidence.push(format!("msg:{subject}(+0.2)"));
            score += 0.2;
        }
        if stats.files > 0
            && stats.lines_added > 0
            && stats.lines_deleted == 0
            && stats.lines_added % stats.files.max(1) == 0
        {
            evidence.push("shape:unusually even additive churn across touched files(+0.2)".into());
            score += 0.2;
        }

        profiles.push(CommitProfile {
            churn,
            message_len: commit.message.len(),
        });

        if evidence.is_empty() || score < 0.3 {
            continue;
        }
        results.push(Attribution {
            author: commit.author.clone(),
            commit_oid: commit.oid.clone(),
            author_time: commit.author_time,
            commit_time: commit.commit_time,
            ai_probability: score.min(0.99),
            evidence,
        });
    }
    results
}

#[derive(Debug, Clone, Copy, Default)]
struct CommitStats {
    lines_added: usize,
    lines_deleted: usize,
    files: usize,
    test_doc_files: usize,
}

#[derive(Debug, Clone, Copy)]
struct CommitProfile {
    churn: usize,
    message_len: usize,
}

fn weak_template_message(message: &str) -> bool {
    let trimmed = message.trim();
    matches!(
        trimmed,
        "update" | "updates" | "improve" | "improvements" | "refactor" | "cleanup" | "fix"
    ) || trimmed.starts_with("update ")
        || trimmed.starts_with("improve ")
        || trimmed.starts_with("refactor ")
}

fn is_large_multifile_burst_commit(
    history: &GitHistory,
    oid: &str,
    time: i64,
    author_key: &str,
    stats: &CommitStats,
) -> bool {
    let churn = stats.lines_added + stats.lines_deleted;
    churn > 100
        && stats.files >= 3
        && history.commits.iter().any(|other| {
            other.oid != oid
                && other.author.identity_key() == author_key
                && (other.commit_time - time).abs() <= 60
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Author, Commit, FileChange, FileChangeKind};
    use std::path::PathBuf;

    fn commit(oid: &str, message: &str, trailers: Vec<&str>, t: i64) -> Commit {
        Commit {
            oid: oid.into(),
            author: Author {
                name: "Dev".into(),
                email: "dev@example.com".into(),
            },
            author_time: t,
            commit_time: t,
            message: message.into(),
            trailers: trailers.into_iter().map(str::to_string).collect(),
        }
    }

    fn change(oid: &str, added: usize, file: &str) -> FileChange {
        FileChange {
            commit_oid: oid.into(),
            path: PathBuf::from(file),
            kind: FileChangeKind::Modify,
            lines_added: added,
            lines_deleted: 0,
        }
    }

    #[test]
    fn strong_ai_trailer_gets_high_probability() {
        let history = GitHistory {
            commits: vec![commit(
                "a",
                "feature",
                vec!["Co-authored-by: Copilot <bot@example.com>"],
                1,
            )],
            changes: vec![change("a", 10, "src/lib.rs")],
            hunks: vec![],
        };
        let attrs = attribute(&history);
        assert!(attrs[0].ai_probability >= 0.9);
    }

    #[test]
    fn medium_large_low_coordination_diff_is_detected() {
        let history = GitHistory {
            commits: vec![commit("a", "add generated helpers", vec![], 1)],
            changes: vec![change("a", 250, "src/lib.rs")],
            hunks: vec![],
        };
        let attrs = attribute(&history);
        assert!(
            attrs[0]
                .evidence
                .iter()
                .any(|e| e.contains("large low-coordination"))
        );
        assert!(attrs[0].ai_probability >= 0.5);
    }

    #[test]
    fn weak_template_message_has_low_probability() {
        let history = GitHistory {
            commits: vec![commit("a", "update", vec![], 1)],
            changes: vec![change("a", 3, "src/lib.rs")],
            hunks: vec![],
        };
        let attrs = attribute(&history);
        assert!(attrs.iter().all(|attr| attr.ai_probability < 0.5));
    }

    #[test]
    fn close_commits_do_not_trigger_burst_without_large_multifile_churn() {
        let history = GitHistory {
            commits: vec![
                commit("a", "small manual edit", vec![], 1),
                commit("b", "another small edit", vec![], 30),
            ],
            changes: vec![change("a", 3, "src/a.rs"), change("b", 4, "src/b.rs")],
            hunks: vec![],
        };

        let attrs = attribute(&history);

        assert!(
            !attrs
                .iter()
                .flat_map(|attr| attr.evidence.iter())
                .any(|evidence| evidence.contains("within 60 seconds"))
        );
    }
}
