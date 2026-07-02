use crate::ai_attribution;
use crate::analyze;
use crate::git_ingest;
use crate::ir::SlopReport;
use crate::scoring;
use anyhow::{Context, Result, anyhow};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn run_demo() -> Result<(PathBuf, SlopReport)> {
    let repo = create_synthetic_repo()?;
    let report = (|| -> Result<SlopReport> {
        let history = git_ingest::parse(&repo)?;
        let analysis = analyze::analyze_repo(&repo, &history)?;
        let attributions = ai_attribution::attribute(&history);
        Ok(scoring::score(&history, analysis.findings, attributions))
    })();
    let cleanup = fs::remove_dir_all(&repo)
        .with_context(|| format!("failed to remove demo repository {}", repo.display()));
    let report = report?;
    cleanup?;
    Ok((repo, report))
}

fn create_synthetic_repo() -> Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let repo = std::env::temp_dir().join(format!("slop-lens-demo-{stamp}"));
    fs::create_dir_all(repo.join("src"))?;
    git(&repo, ["init"])?;
    git(&repo, ["config", "user.name", "Demo Human"])?;
    git(&repo, ["config", "user.email", "human@example.com"])?;

    write(
        &repo,
        "src/lib.rs",
        "pub fn parse(input: &str) -> Vec<&str> {\n    input.split_whitespace().collect()\n}\n",
    )?;
    commit(&repo, "init parser", 1)?;

    append(
        &repo,
        "src/lib.rs",
        "\npub fn normalize(input: &str) -> String {\n    input.trim().to_ascii_lowercase()\n}\n",
    )?;
    commit(&repo, "add normalization", 2)?;

    write(
        &repo,
        "src/scoring.rs",
        "pub fn score(items: &[i32]) -> i32 {\n    items.iter().sum()\n}\n",
    )?;
    commit(&repo, "add scoring", 3)?;

    append(
        &repo,
        "src/scoring.rs",
        "\npub fn clamp_score(value: i32) -> i32 {\n    if value < 0 { 0 } else if value > 100 { 100 } else { value }\n}\n",
    )?;
    commit(&repo, "human small scoring fix", 4)?;

    let generated = generated_blob();
    write(&repo, "src/generated.rs", &generated)?;
    commit_with_message(
        &repo,
        "update\n\nCo-authored-by: Copilot <copilot@users.noreply.github.com>",
        5,
    )?;

    append(
        &repo,
        "src/lib.rs",
        "\nmod generated;\n\npub fn public_api(input: &str) -> usize {\n    parse(input).len()\n}\n",
    )?;
    commit(&repo, "wire generated module", 6)?;

    append(
        &repo,
        "src/generated.rs",
        "\nfn dead_ai_helper_alpha() -> i32 { 42 }\nfn dead_ai_helper_beta() -> i32 { 7 }\n",
    )?;
    commit_with_message(&repo, "improve helpers\n\nAI-Assisted: true", 7)?;

    write(
        &repo,
        "src/workflow.rs",
        &format!("{}\n{}", duplicate_fn("copy_a"), duplicate_fn("copy_b")),
    )?;
    commit(&repo, "refactor workflow", 8)?;

    append(
        &repo,
        "src/workflow.rs",
        "\npub fn route(value: i32) -> i32 {\n    if value > 10 { value } else { copy_a(&[value]) }\n}\n",
    )?;
    commit(&repo, "human route workflow", 9)?;

    write(&repo, "src/noisy.rs", &comment_heavy_file())?;
    commit_with_message(&repo, "update comments", 10)?;

    append(
        &repo,
        "src/lib.rs",
        "\nmod scoring;\nmod workflow;\nmod noisy;\n",
    )?;
    commit(&repo, "release wiring", 11)?;

    append(
        &repo,
        "src/scoring.rs",
        "\npub fn score_one(value: i32) -> i32 { clamp_score(value) }\n",
    )?;
    commit(&repo, "small release fix", 12)?;

    append(
        &repo,
        "src/generated.rs",
        "\nfn never_called_release_padding() -> i32 { dead_ai_helper_alpha() + 1 }\n",
    )?;
    commit_with_message(&repo, "cleanup", 13)?;

    append(
        &repo,
        "src/workflow.rs",
        "\npub fn route_two(value: i32) -> i32 {\n    route(value) + copy_b(&[value, value + 1])\n}\n",
    )?;
    commit(&repo, "manual integration", 14)?;

    append(
        &repo,
        "src/lib.rs",
        "\npub fn release_name() -> &'static str { \"demo\" }\n",
    )?;
    commit(&repo, "release", 15)?;

    append(
        &repo,
        "src/noisy.rs",
        "\npub fn noisy_value() -> i32 { documented_behavior_12() }\n",
    )?;
    commit(&repo, "post release touch", 16)?;

    Ok(repo)
}

fn generated_blob() -> String {
    let mut out = String::new();
    out.push_str(
        "pub fn generated_entry(input: i32) -> i32 {\n    generated_helper_0(input)\n}\n\n",
    );
    for i in 0..36 {
        out.push_str(&format!(
            "fn generated_helper_{i}(mut value: i32) -> i32 {{\n    if value > {i} {{ value += {i}; }} else {{ value -= {i}; }}\n    value\n}}\n\n"
        ));
    }
    out.push_str("pub fn generated_complex(mut value: i32) -> i32 {\n");
    for i in 0..14 {
        out.push_str(&format!("    if value > {i} {{ value -= {i}; }}\n"));
    }
    out.push_str("    value\n}\n");
    out
}

fn duplicate_fn(name: &str) -> String {
    format!(
        "pub fn {name}(input: &[i32]) -> i32 {{\n    let mut total = 0;\n    for value in input {{\n        if *value > 10 {{\n            total += *value * 2;\n        }} else {{\n            total += *value + 1;\n        }}\n    }}\n    total\n}}\n"
    )
}

fn comment_heavy_file() -> String {
    let mut out = String::new();
    for i in 0..34 {
        out.push_str(&format!("// generated explanation block {i}\n"));
    }
    for i in 0..30 {
        out.push_str(&format!(
            "pub fn documented_behavior_{i}() -> i32 {{ {i} }}\n"
        ));
    }
    out
}

fn write(repo: &Path, path: &str, contents: &str) -> Result<()> {
    let full = repo.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(full, contents)?;
    Ok(())
}

fn append(repo: &Path, path: &str, contents: &str) -> Result<()> {
    let full = repo.join(path);
    let mut current = fs::read_to_string(&full).unwrap_or_default();
    current.push_str(contents);
    fs::write(full, current)?;
    Ok(())
}

fn commit(repo: &Path, message: &str, index: i64) -> Result<()> {
    commit_with_message(repo, message, index)
}

fn commit_with_message(repo: &Path, message: &str, index: i64) -> Result<()> {
    git(repo, ["add", "."])?;
    let timestamp = 1_700_000_000 + index * 60;
    let output = Command::new("git")
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", format!("{timestamp} +0000"))
        .env("GIT_COMMITTER_DATE", format!("{timestamp} +0000"))
        .current_dir(repo)
        .output()
        .context("failed to execute git commit")?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn git<const N: usize>(repo: &Path, args: [&str; N]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .with_context(|| format!("failed to execute git {}", args.join(" ")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_demo_removes_temp_repo() {
        let (repo, report) = run_demo().unwrap();

        assert!(!repo.exists());
        assert!(report.summary.commit_count > 0);
    }
}
