use crate::ir::{Author, Commit, FileChange, FileChangeKind, Hunk};
use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const SUPPORTED_PATHSPECS: [&str; 5] = ["*.py", "*.go", "*.rs", "*.ts", "*.js"];

#[derive(Debug, Clone, Default)]
pub struct GitHistory {
    pub repo_url: Option<String>,
    pub commits: Vec<Commit>,
    pub changes: Vec<FileChange>,
    pub hunks: Vec<Hunk>,
}

pub fn parse(repo_path: &Path) -> Result<GitHistory> {
    parse_range(repo_path, None, None)
}

pub fn parse_range(repo_path: &Path, from: Option<&str>, to: Option<&str>) -> Result<GitHistory> {
    parse_range_with_progress(repo_path, from, to, true)
}

pub fn parse_range_with_progress(
    repo_path: &Path,
    from: Option<&str>,
    to: Option<&str>,
    emit_progress: bool,
) -> Result<GitHistory> {
    ensure_git_repo(repo_path)?;
    let repo_url = repo_remote_url(repo_path);
    let commits = read_commits(repo_path, from, to)?;
    if emit_progress {
        eprintln!("running git log (1/3): name-status...");
    }
    let status_by_commit = read_batch_status(repo_path, from, to)?;
    if emit_progress {
        eprintln!("running git log (2/3): numstat...");
    }
    let numstat_by_commit = read_batch_numstat(repo_path, from, to)?;
    if emit_progress {
        eprintln!("running git log (3/3): patch...");
    }
    let mut hunks_by_commit = read_batch_hunks(repo_path, from, to)?;

    let mut changes = Vec::new();
    let mut hunks = Vec::new();
    for commit in &commits {
        changes.extend(changes_for_commit(
            &commit.oid,
            status_by_commit
                .get(commit.oid.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            numstat_by_commit
                .get(commit.oid.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        ));
        if let Some(mut commit_hunks) = hunks_by_commit.remove(commit.oid.as_str()) {
            hunks.append(&mut commit_hunks);
        }
    }

    Ok(GitHistory {
        repo_url,
        commits,
        changes,
        hunks,
    })
}

pub fn repo_remote_url(repo_path: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_github_remote_url(String::from_utf8_lossy(&output.stdout).trim())
}

fn parse_github_remote_url(remote: &str) -> Option<String> {
    let remote = remote.trim();
    let path = remote
        .strip_prefix("git@github.com:")
        .or_else(|| remote.strip_prefix("https://github.com/"))
        .or_else(|| remote.strip_prefix("http://github.com/"))
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))?;
    let path = path
        .trim_end_matches('/')
        .strip_suffix(".git")
        .unwrap_or_else(|| path.trim_end_matches('/'));
    let mut parts = path.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(format!("https://github.com/{owner}/{repo}"))
}

fn ensure_git_repo(repo_path: &Path) -> Result<()> {
    if !repo_path.exists() {
        return Err(anyhow!(
            "{} does not exist. hint: pass --repo with an existing git working tree",
            repo_path.display()
        ));
    }
    if !repo_path.is_dir() {
        return Err(anyhow!(
            "{} is not a directory. hint: pass --repo with a git working tree",
            repo_path.display()
        ));
    }
    let out = git(repo_path, ["rev-parse", "--git-dir"])?;
    if out.trim().is_empty() {
        Err(anyhow!(
            "{} is not a git repository. hint: run this command inside a git checkout or pass --repo /path/to/repo",
            repo_path.display()
        ))
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

#[derive(Debug, Clone)]
struct StatusChange {
    path: PathBuf,
    kind: FileChangeKind,
}

#[derive(Debug, Clone)]
struct NumstatChange {
    path: PathBuf,
    lines_added: usize,
    lines_deleted: usize,
}

fn read_batch_status(
    repo_path: &Path,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<HashMap<String, Vec<Option<StatusChange>>>> {
    let mut args = vec![
        "log".to_string(),
        "--reverse".to_string(),
        "--find-renames".to_string(),
        "--name-status".to_string(),
        "--format=%x00%H%x1e".to_string(),
    ];
    push_range_and_pathspec(&mut args, from, to);
    let output = git_owned(repo_path, args)?;
    let mut by_commit = HashMap::new();
    for (oid, body) in split_commit_log(&output) {
        let mut status_changes = Vec::new();
        for line in body.lines().filter(|line| !line.trim().is_empty()) {
            status_changes.push(parse_name_status_line(line));
        }
        by_commit.insert(oid.to_string(), status_changes);
    }
    Ok(by_commit)
}

fn read_batch_numstat(
    repo_path: &Path,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<HashMap<String, Vec<NumstatChange>>> {
    let mut args = vec![
        "log".to_string(),
        "--reverse".to_string(),
        "--find-renames".to_string(),
        "--numstat".to_string(),
        "--format=%x00%H%x1e".to_string(),
    ];
    push_range_and_pathspec(&mut args, from, to);
    let output = git_owned(repo_path, args)?;
    let mut by_commit = HashMap::new();
    for (oid, body) in split_commit_log(&output) {
        let mut numstat_changes = Vec::new();
        for line in body.lines().filter(|line| !line.trim().is_empty()) {
            let mut parts = line.split('\t');
            let added = parse_numstat(parts.next().unwrap_or_default());
            let deleted = parse_numstat(parts.next().unwrap_or_default());
            let raw_path = parts.collect::<Vec<_>>().join("\t");
            let Some(path) = parse_numstat_path(&raw_path)
                .filter(|path| is_supported_path(&path.to_string_lossy()))
            else {
                continue;
            };
            numstat_changes.push(NumstatChange {
                path,
                lines_added: added,
                lines_deleted: deleted,
            });
        }
        by_commit.insert(oid.to_string(), numstat_changes);
    }
    Ok(by_commit)
}

fn read_batch_hunks(
    repo_path: &Path,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<HashMap<String, Vec<Hunk>>> {
    let mut args = vec![
        "log".to_string(),
        "--reverse".to_string(),
        "--find-renames".to_string(),
        "-p".to_string(),
        "--unified=0".to_string(),
        "--format=%x00%H%x1e".to_string(),
    ];
    push_range_and_pathspec(&mut args, from, to);
    let output = git_owned(repo_path, args)?;
    let mut by_commit = HashMap::new();
    for (oid, body) in split_commit_log(&output) {
        by_commit.insert(oid.to_string(), parse_hunks_for_commit(oid, body));
    }
    Ok(by_commit)
}

fn parse_name_status_line(line: &str) -> Option<StatusChange> {
    let mut parts = line.split('\t');
    let status = parts.next().unwrap_or_default();
    let kind = match status.chars().next().unwrap_or('M') {
        'A' => FileChangeKind::Add,
        'D' => FileChangeKind::Delete,
        'R' => FileChangeKind::Rename,
        _ => FileChangeKind::Modify,
    };
    let path = if kind == FileChangeKind::Rename {
        let _old_path = parts.next();
        parts.next().unwrap_or_default()
    } else {
        parts.next().unwrap_or_default()
    };
    if path.is_empty() || !is_supported_path(path) {
        return None;
    }
    Some(StatusChange {
        path: PathBuf::from(path),
        kind,
    })
}

fn changes_for_commit(
    oid: &str,
    status_changes: &[Option<StatusChange>],
    numstat_changes: &[NumstatChange],
) -> Vec<FileChange> {
    let mut changes = Vec::new();
    for (idx, numstat) in numstat_changes.iter().enumerate() {
        let status_change = status_changes.get(idx).and_then(Option::as_ref);
        let path = numstat.path.clone();
        let kind = status_change
            .filter(|status| status.path == path)
            .map(|status| status.kind.clone())
            .unwrap_or(FileChangeKind::Modify);
        changes.push(FileChange {
            commit_oid: oid.to_string(),
            path,
            kind,
            lines_added: numstat.lines_added,
            lines_deleted: numstat.lines_deleted,
        });
    }
    changes
}

fn parse_hunks_for_commit(oid: &str, diff: &str) -> Vec<Hunk> {
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
    hunks
}

fn split_commit_log(output: &str) -> impl Iterator<Item = (&str, &str)> {
    output.split('\0').filter_map(|raw| {
        let raw = raw.trim_start_matches('\n');
        if raw.is_empty() {
            return None;
        }
        let (oid, body) = raw.split_once('\x1e')?;
        let oid = oid.trim();
        if oid.len() < 7 {
            return None;
        }
        Some((oid, body.trim_start_matches('\n')))
    })
}

fn push_range_and_pathspec(args: &mut Vec<String>, from: Option<&str>, to: Option<&str>) {
    match (from, to) {
        (Some(from), Some(to)) => args.push(format!("{from}..{to}")),
        (None, Some(to)) => args.push(to.to_string()),
        (Some(from), None) => args.push(format!("{from}..HEAD")),
        (None, None) => {}
    }
    args.push("--".to_string());
    args.extend(
        SUPPORTED_PATHSPECS
            .iter()
            .map(|pathspec| pathspec.to_string()),
    );
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
        || lower.starts_with("generated with:")
        || lower.starts_with("generated with ")
        || lower.starts_with("refactored-by:")
        || lower.starts_with("refactored by:")
        || lower.starts_with("refactored by ")
        || lower.starts_with("ai-assisted:")
        || lower.starts_with("tool:")
}

fn parse_numstat(value: &str) -> usize {
    value.parse::<usize>().unwrap_or(0)
}

fn parse_numstat_path(raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if let Some((_, new_path)) = raw.rsplit_once('\t') {
        return Some(PathBuf::from(new_path));
    }
    if raw.contains(" => ") {
        return Some(PathBuf::from(rename_target_path(raw)));
    }
    Some(PathBuf::from(raw))
}

fn rename_target_path(path: &str) -> String {
    if let (Some(open), Some(close)) = (path.find('{'), path.find('}')) {
        let prefix = &path[..open];
        let suffix = &path[close + 1..];
        let inner = &path[open + 1..close];
        if let Some((_, to)) = inner.split_once(" => ") {
            return format!("{prefix}{}{suffix}", to.trim());
        }
    }
    path.rsplit_once(" => ")
        .map(|(_, to)| to.trim().to_string())
        .unwrap_or_else(|| path.to_string())
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
        return Err(anyhow!("{}", friendly_git_error(repo_path, &args, &stderr)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn friendly_git_error(repo_path: &Path, args: &[String], stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.contains("not a git repository") {
        return format!(
            "{} is not a git repository. hint: run this command inside a git checkout or pass --repo /path/to/repo",
            repo_path.display()
        );
    }
    if stderr.contains("does not have any commits yet")
        || stderr.contains("your current branch")
        || stderr.contains("bad default revision 'HEAD'")
    {
        return format!(
            "{} has no commits yet. hint: make an initial commit before scanning",
            repo_path.display()
        );
    }
    if is_revision_error(stderr) {
        if let Some(revision) = revision_arg(args) {
            let revision = revision_label(revision);
            return format!(
                "revision '{revision}' not found. hint: check the spelling or fetch the branch/tag before scanning"
            );
        }
        return "requested revision was not found. hint: check the spelling or fetch the branch/tag before scanning".into();
    }
    let detail = clean_git_stderr(stderr);
    if detail.is_empty() {
        format!("git {} failed", args.join(" "))
    } else {
        format!("git {} failed: {}", args.join(" "), detail)
    }
}

fn is_revision_error(stderr: &str) -> bool {
    stderr.contains("ambiguous argument")
        || stderr.contains("unknown revision")
        || stderr.contains("bad revision")
        || stderr.contains("invalid object name")
        || stderr.contains("Needed a single revision")
}

fn revision_arg(args: &[String]) -> Option<&str> {
    let mut revision = None;
    for arg in args.iter().skip(1) {
        if arg == "--" {
            break;
        }
        if !arg.starts_with('-') {
            revision = Some(arg.as_str());
        }
    }
    revision
}

fn revision_label(revision: &str) -> &str {
    let Some((from, to)) = revision.split_once("..") else {
        return revision;
    };
    match (from, to) {
        ("", "") => revision,
        ("", to) => to,
        (from, "") | (from, "HEAD") => from,
        _ => revision,
    }
}

fn clean_git_stderr(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .trim_start_matches("fatal: ")
        .trim_start_matches("error: ")
        .to_string()
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
    fn parses_renames_as_single_rename_change() {
        let repo = temp_repo();
        fs::write(repo.join("old.py"), "def used():\n    return 1\n").unwrap();
        commit(&repo, "init");
        Command::new("git")
            .args(["mv", "old.py", "new.py"])
            .current_dir(&repo)
            .output()
            .unwrap();
        commit(&repo, "rename file");

        let history = parse(&repo).unwrap();
        let rename_commit = history
            .commits
            .iter()
            .find(|commit| commit.message == "rename file")
            .unwrap();
        let changes = history
            .changes
            .iter()
            .filter(|change| change.commit_oid == rename_commit.oid)
            .collect::<Vec<_>>();

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path, Path::new("new.py"));
        assert_eq!(changes[0].kind, FileChangeKind::Rename);
        assert_eq!(changes[0].lines_added + changes[0].lines_deleted, 0);
    }

    #[test]
    fn filters_supported_paths() {
        assert!(is_supported_path("src/main.rs"));
        assert!(is_supported_path("cmd/main.go"));
        assert!(!is_supported_path("README.md"));
    }

    #[test]
    fn parses_github_remote_urls() {
        assert_eq!(
            parse_github_remote_url("git@github.com:Loping151/XutheringWavesUID.git"),
            Some("https://github.com/Loping151/XutheringWavesUID".into())
        );
        assert_eq!(
            parse_github_remote_url("https://github.com/Loping151/XutheringWavesUID.git"),
            Some("https://github.com/Loping151/XutheringWavesUID".into())
        );
        assert_eq!(
            parse_github_remote_url("ssh://git@github.com/Loping151/XutheringWavesUID"),
            Some("https://github.com/Loping151/XutheringWavesUID".into())
        );
        assert_eq!(parse_github_remote_url("git@example.com:x/y.git"), None);
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
