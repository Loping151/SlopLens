use crate::git_ingest::GitHistory;
use crate::ir::Attribution;
use std::collections::{HashMap, HashSet};

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
    let burst_commits = large_multifile_burst_commits(history, &by_commit);

    let mut author_history: HashMap<String, Vec<CommitProfile>> = HashMap::new();
    let mut results = Vec::new();
    for commit in &history.commits {
        let stats = by_commit
            .get(commit.oid.as_str())
            .cloned()
            .unwrap_or_default();
        let mut evidence = Vec::new();
        let mut score = 0.0f64;
        let mut has_strong_evidence = false;

        let mut seen_trailers = HashSet::new();
        for trailer in commit
            .trailers
            .iter()
            .filter(|trailer| seen_trailers.insert(trailer.trim().to_ascii_lowercase()))
        {
            let lower = trailer.to_ascii_lowercase();
            if has_ai_keyword(&lower) || lower.contains("bot") || lower.contains("github.dev") {
                evidence.push(format!("trailer:{trailer}(+0.9)"));
                score += 0.9;
                has_strong_evidence = true;
            }
        }
        let lower_message = commit.message.to_ascii_lowercase();
        if has_ai_keyword(&lower_message)
            || commit
                .author
                .email
                .to_ascii_lowercase()
                .contains("users.noreply.github.dev")
        {
            evidence.push("metadata:AI or GitHub web authoring(+0.9)".into());
            score += 0.9;
            has_strong_evidence = true;
        }

        let churn = stats.lines_added + stats.lines_deleted;
        if churn > 200 && stats.files <= 2 {
            evidence.push(format!(
                "churn:large low-coordination diff ({churn} changed lines across {} files)(+0.5)",
                stats.files
            ));
            score += 0.5;
            has_strong_evidence = true;
        }
        if burst_commits.contains(commit.oid.as_str()) {
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
        if has_strong_evidence && profiles.len() >= 2 {
            let avg_churn =
                profiles.iter().map(|p| p.churn as f64).sum::<f64>() / profiles.len() as f64;
            let avg_msg =
                profiles.iter().map(|p| p.message_len as f64).sum::<f64>() / profiles.len() as f64;
            if (churn as f64) > (avg_churn * 3.0).max(120.0)
                || (commit.message.len() as f64) > (avg_msg * 3.0).max(80.0)
            {
                evidence.push(
                    "style:author style/churn changed sharply versus prior commits(+0.15)".into(),
                );
                score += 0.15;
            }
        }

        if weak_template_message(&lower_message) {
            let subject = lower_message.lines().next().unwrap_or_default().trim();
            evidence.push(format!("msg:{subject}(+0.2)"));
            score += 0.2;
        }
        profiles.push(CommitProfile {
            churn,
            message_len: commit.message.len(),
        });

        if evidence.is_empty() || score < 0.5 || !has_strong_evidence {
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

fn has_ai_keyword(text: &str) -> bool {
    [
        "copilot",
        "claude",
        "cursor",
        "gpt",
        "gemini",
        "chatgpt",
        "deepseek",
        "generated-by: ai",
        "generated by ai",
        "generated with",
        "ai-assisted",
        "refactored by",
        "users.noreply.github.dev",
    ]
    .iter()
    .any(|keyword| text.contains(keyword))
}

fn large_multifile_burst_commits<'a>(
    history: &'a GitHistory,
    by_commit: &HashMap<&str, CommitStats>,
) -> HashSet<&'a str> {
    let mut by_author: HashMap<String, Vec<(&'a str, i64)>> = HashMap::new();
    for commit in &history.commits {
        by_author
            .entry(commit.author.identity_key())
            .or_default()
            .push((commit.oid.as_str(), commit.commit_time));
    }

    let mut burst_commits = HashSet::new();
    for commits in by_author.values_mut() {
        commits.sort_by_key(|(_, commit_time)| *commit_time);
        let mut left = 0usize;
        let mut right = 0usize;
        for idx in 0..commits.len() {
            while commits[idx].1 - commits[left].1 > 60 {
                left += 1;
            }
            while right < commits.len() && commits[right].1 - commits[idx].1 <= 60 {
                right += 1;
            }
            if right.saturating_sub(left) > 1
                && by_commit
                    .get(commits[idx].0)
                    .is_some_and(is_large_multifile_commit)
            {
                burst_commits.insert(commits[idx].0);
            }
        }
    }
    burst_commits
}

fn is_large_multifile_commit(stats: &CommitStats) -> bool {
    stats.lines_added + stats.lines_deleted > 100 && stats.files >= 3
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
            repo_url: None,
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
            repo_url: None,
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
            repo_url: None,
            commits: vec![commit("a", "update", vec![], 1)],
            changes: vec![change("a", 3, "src/lib.rs")],
            hunks: vec![],
        };
        let attrs = attribute(&history);
        assert!(attrs.iter().all(|attr| attr.ai_probability < 0.5));
    }

    #[test]
    fn duplicate_ai_trailers_count_once() {
        let trailer = "Co-authored-by: Copilot <bot@example.com>";
        let history = GitHistory {
            repo_url: None,
            commits: vec![commit("a", "feature", vec![trailer, trailer], 1)],
            changes: vec![change("a", 10, "src/lib.rs")],
            hunks: vec![],
        };

        let attrs = attribute(&history);

        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].evidence.len(), 1);
        assert_eq!(attrs[0].ai_probability, 0.9);
    }

    #[test]
    fn style_change_without_strong_evidence_does_not_enter_results() {
        let history = GitHistory {
            repo_url: None,
            commits: vec![
                commit("a", "small manual edit", vec![], 1),
                commit("b", "another manual edit", vec![], 2),
                commit(
                    "c",
                    "long manual message that is intentionally much longer than the prior messages but has no ai metadata",
                    vec![],
                    3,
                ),
            ],
            changes: vec![
                change("a", 10, "src/a.rs"),
                change("b", 12, "src/b.rs"),
                change("c", 150, "src/c.rs"),
            ],
            hunks: vec![],
        };

        let attrs = attribute(&history);

        assert!(attrs.is_empty());
    }

    #[test]
    fn expanded_ai_keywords_are_detected() {
        let history = GitHistory {
            repo_url: None,
            commits: vec![commit("a", "generated with claude", vec![], 1)],
            changes: vec![change("a", 3, "src/lib.rs")],
            hunks: vec![],
        };

        let attrs = attribute(&history);

        assert_eq!(attrs.len(), 1);
        assert!(attrs[0].ai_probability >= 0.9);
    }

    #[test]
    fn close_commits_do_not_trigger_burst_without_large_multifile_churn() {
        let history = GitHistory {
            repo_url: None,
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
