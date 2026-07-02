use crate::ir::{Author, Commit, FileChange, FileChangeKind, Hunk};
use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, Default)]
pub struct GitHistory {
    pub commits: Vec<Commit>,
    pub changes: Vec<FileChange>,
    pub hunks: Vec<Hunk>,
}

pub fn parse(repo_path: &Path) -> Result<GitHistory> {
    parse_range(repo_path, None, None)
}

pub fn parse_range(repo_path: &Path, from: Option<&str>, to: Option<&str>) -> Result<GitHistory> {
    ensure_git_repo(repo_path)?;
    let commits = read_commits(repo_path, from, to)?;
    let mut changes = Vec::new();
    let mut hunks = Vec::new();

    for commit in &commits {
        changes.extend(read_commit_changes(repo_path, &commit.oid)?);
        hunks.extend(read_commit_hunks(repo_path, &commit.oid)?);
    }

    Ok(GitHistory {
        commits,
        changes,
        hunks,
    })
}

fn ensure_git_repo(repo_path: &Path) -> Result<()> {
    let out = git(repo_path, ["rev-parse", "--git-dir"])?;
    if out.trim().is_empty() {
        Err(anyhow!("not a git repository: {}", repo_path.display()))
    } else {
        Ok(())
    }
}

fn read_commits(repo_path: &Path, from: Option<&str>, to: Option<&str>) -> Result<Vec<Commit>> {
    let mut args = vec![
        "log".to_string(),
        "--reverse".to_string(),
        "--date=unix".to_string(),
        "-z".to_string(),
        "--pretty=format:%H%x00%an%x00%ae%x00%at%x00%ct%x00%B%x00%x1e".to_string(),
    ];
    match (from, to) {
        (Some(from), Some(to)) => args.push(format!("{from}..{to}")),
        (None, Some(to)) => args.push(to.to_string()),
        (Some(from), None) => args.push(format!("{from}..HEAD")),
        (None, None) => {}
    }

    let output = git_owned(repo_path, args)?;
    let mut commits = Vec::new();
    for raw in output.split('\x1e') {
        let raw = raw.trim_matches('\0').trim();
        if raw.is_empty() {
            continue;
        }
        let mut parts = raw.splitn(6, '\0');
        let oid = parts.next().unwrap_or_default().trim().to_string();
        let name = parts.next().unwrap_or_default().to_string();
        let email = parts.next().unwrap_or_default().to_string();
        let author_time = parts.next().unwrap_or("0").parse().unwrap_or(0);
        let commit_time = parts.next().unwrap_or("0").parse().unwrap_or(0);
        let message = parts
            .next()
            .unwrap_or_default()
            .trim_matches('\0')
            .trim()
            .to_string();
        if oid.len() < 7 {
            continue;
        }
        let trailers = message
            .lines()
            .filter(|line| looks_like_trailer(line))
            .map(str::trim)
            .map(str::to_string)
            .collect();
        commits.push(Commit {
            oid,
            author: Author { name, email },
            author_time,
            commit_time,
            message,
            trailers,
        });
    }
    Ok(commits)
}

fn read_commit_changes(repo_path: &Path, oid: &str) -> Result<Vec<FileChange>> {
    let mut kinds = HashMap::new();
    let status = git(
        repo_path,
        [
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-status",
            "-r",
            "--no-renames",
            oid,
        ],
    )?;
    for line in status.lines() {
        let mut parts = line.splitn(2, '\t');
        let status = parts.next().unwrap_or_default();
        let path = parts.next().unwrap_or_default();
        if path.is_empty() || !is_supported_path(path) {
            continue;
        }
        let kind = match status.chars().next().unwrap_or('M') {
            'A' => FileChangeKind::Add,
            'D' => FileChangeKind::Delete,
            _ => FileChangeKind::Modify,
        };
        kinds.insert(PathBuf::from(path), kind);
    }

    let numstat = git(
        repo_path,
        ["show", "--numstat", "--format=", "--no-renames", oid],
    )?;
    let mut changes = Vec::new();
    for line in numstat.lines() {
        let mut parts = line.split('\t');
        let added = parse_numstat(parts.next().unwrap_or_default());
        let deleted = parse_numstat(parts.next().unwrap_or_default());
        let path = parts.next().unwrap_or_default();
        if path.is_empty() || !is_supported_path(path) {
            continue;
        }
        let path = PathBuf::from(path);
        let kind = kinds.get(&path).cloned().unwrap_or(FileChangeKind::Modify);
        changes.push(FileChange {
            commit_oid: oid.to_string(),
            path,
            kind,
            lines_added: added,
            lines_deleted: deleted,
        });
    }
    Ok(changes)
}

fn read_commit_hunks(repo_path: &Path, oid: &str) -> Result<Vec<Hunk>> {
    let diff = git(
        repo_path,
        ["show", "--format=", "--unified=0", "--no-renames", oid],
    )?;
    let mut hunks = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current: Option<Hunk> = None;

    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current_path = is_supported_path(path).then(|| PathBuf::from(path));
            continue;
        }
        if line.starts_with("@@ ") {
            if let Some(hunk) = current.take()
                && (!hunk.added.is_empty() || !hunk.removed.is_empty())
            {
                hunks.push(hunk);
            }
            let start_line = parse_hunk_start(line);
            if let Some(path) = current_path.clone() {
                current = Some(Hunk {
                    path,
                    commit_oid: oid.to_string(),
                    start_line,
                    added: Vec::new(),
                    removed: Vec::new(),
                });
            } else {
                current = None;
            }
            continue;
        }
        if let Some(hunk) = current.as_mut() {
            if line.starts_with('+') && !line.starts_with("+++") {
                hunk.added.push(line[1..].to_string());
            } else if line.starts_with('-') && !line.starts_with("---") {
                hunk.removed.push(line[1..].to_string());
            }
        }
    }
    if let Some(hunk) = current.take()
        && (!hunk.added.is_empty() || !hunk.removed.is_empty())
    {
        hunks.push(hunk);
    }
    Ok(hunks)
}

fn parse_hunk_start(line: &str) -> usize {
    line.split_whitespace()
        .find(|part| part.starts_with('+'))
        .and_then(|part| {
            part.trim_start_matches('+')
                .split(',')
                .next()
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(1)
}

fn looks_like_trailer(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.starts_with("co-authored-by:")
        || lower.starts_with("signed-off-by:")
        || lower.starts_with("generated-by:")
        || lower.starts_with("ai-assisted:")
        || lower.starts_with("tool:")
}

fn parse_numstat(value: &str) -> usize {
    value.parse::<usize>().unwrap_or(0)
}

pub fn is_supported_path(path: &str) -> bool {
    matches!(
        Path::new(path).extension().and_then(|ext| ext.to_str()),
        Some("rs" | "go" | "py" | "js" | "ts")
    )
}

fn git<const N: usize>(repo_path: &Path, args: [&str; N]) -> Result<String> {
    git_owned(repo_path, args.into_iter().map(str::to_string).collect())
}

fn git_owned(repo_path: &Path, args: Vec<String>) -> Result<String> {
    let output = Command::new("git")
        .args(&args)
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("failed to execute git {}", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("git {} failed: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_repo() -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("slop-lens-git-test-{stamp}"));
        fs::create_dir_all(&path).unwrap();
        Command::new("git")
            .arg("init")
            .current_dir(&path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "Tester"])
            .current_dir(&path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["config", "user.email", "tester@example.com"])
            .current_dir(&path)
            .output()
            .unwrap();
        path
    }

    fn commit(repo: &Path, message: &str) {
        Command::new("git")
            .arg("add")
            .arg(".")
            .current_dir(repo)
            .output()
            .unwrap();
        let out = Command::new("git")
            .args(["commit", "-m", message])
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn parses_commit_changes_and_hunks() {
        let repo = temp_repo();
        fs::write(repo.join("lib.rs"), "fn used() {}\n").unwrap();
        commit(&repo, "init");
        fs::write(repo.join("lib.rs"), "fn used() {}\nfn extra() {}\n").unwrap();
        commit(&repo, "add extra");

        let history = parse(&repo).unwrap();
        assert_eq!(history.commits.len(), 2);
        assert!(
            history
                .changes
                .iter()
                .any(|c| c.path == Path::new("lib.rs"))
        );
        assert!(
            history
                .hunks
                .iter()
                .any(|h| h.added.iter().any(|l| l.contains("extra")))
        );
    }

    #[test]
    fn filters_supported_paths() {
        assert!(is_supported_path("src/main.rs"));
        assert!(is_supported_path("cmd/main.go"));
        assert!(!is_supported_path("README.md"));
    }

    #[test]
    fn default_parse_reads_current_branch_only() {
        let repo = temp_repo();
        fs::write(repo.join("lib.rs"), "fn main_branch() {}\n").unwrap();
        commit(&repo, "main commit");
        let current_branch = git(&repo, ["branch", "--show-current"]).unwrap();
        Command::new("git")
            .args(["checkout", "-b", "other"])
            .current_dir(&repo)
            .output()
            .unwrap();
        fs::write(repo.join("lib.rs"), "fn other_branch() {}\n").unwrap();
        commit(&repo, "other commit");
        Command::new("git")
            .args(["checkout", current_branch.trim()])
            .current_dir(&repo)
            .output()
            .unwrap();

        let history = parse(&repo).unwrap();
        assert_eq!(history.commits.len(), 1);
        assert_eq!(history.commits[0].message, "main commit");
    }
}
