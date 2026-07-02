use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use slop_lens::ai_attribution;
use slop_lens::analyze;
use slop_lens::baseline;
use slop_lens::config::SlopConfig;
use slop_lens::demo;
use slop_lens::git_ingest;
use slop_lens::git_ingest::GitHistory;
use slop_lens::ir::{Finding, FindingStatus, RangeSummary, SlopReport};
use slop_lens::report::{self, ReportFormat};
use slop_lens::scoring;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(
    name = "slop-lens",
    version,
    about = "AI code debt scanner for git repositories"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Scan {
        #[arg(long, default_value = ".", help = "Path to the git repository to scan")]
        repo: PathBuf,
        #[arg(
            long,
            help = "Start commit or revision for a range scan; scans changed files in from..to"
        )]
        from: Option<String>,
        #[arg(
            long,
            help = "End commit or revision for a range scan; defaults to HEAD when --from is set"
        )]
        to: Option<String>,
        #[arg(
            long,
            value_enum,
            default_value_t = CliFormat::Terminal,
            help = "Report format to render"
        )]
        format: CliFormat,
        #[arg(long, help = "Write the report to a file instead of stdout")]
        out: Option<PathBuf>,
        #[arg(
            long,
            value_enum,
            help = "Fail the scan when findings reach this level"
        )]
        fail_on: Option<FailOn>,
        #[arg(long, value_parser = parse_non_negative_f64, help = "Fail the scan when total debt exceeds this score")]
        max_debt: Option<f64>,
    },
    Baseline {
        #[arg(long, default_value = ".", help = "Path to the git repository to scan")]
        repo: PathBuf,
        #[arg(long, help = "Save the current scan as .slop/baseline.json")]
        save: bool,
    },
    Demo {
        #[arg(
            long,
            value_enum,
            default_value_t = CliFormat::Terminal,
            help = "Report format to render"
        )]
        format: CliFormat,
        #[arg(long, help = "Write the report to a file instead of stdout")]
        out: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliFormat {
    Terminal,
    Html,
    Json,
    Sarif,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum FailOn {
    Error,
    Warning,
}

fn main() -> ExitCode {
    match run() {
        Ok(exit_code) => exit_code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    match cli.command {
        Command::Scan {
            repo,
            from,
            to,
            format,
            out,
            fail_on,
            max_debt,
        } => {
            let mut report = build_report(&repo, from.as_deref(), to.as_deref())?;
            if let Some(saved) = baseline::load_if_exists(&repo)? {
                baseline::apply(&mut report, &saved);
            }
            write_report(&report, format.into(), out)?;
            if gate_failed(&report, fail_on, max_debt) {
                return Ok(ExitCode::from(1));
            }
        }
        Command::Baseline { repo, save } => {
            if !save {
                anyhow::bail!("baseline currently requires --save");
            }
            let report = build_report(&repo, None, None)?;
            let path = baseline::save(&repo, &report)?;
            eprintln!("baseline saved: {}", path.display());
        }
        Command::Demo { format, out } => {
            eprintln!("building synthetic demo repository...");
            let (repo, report) = demo::run_demo()?;
            eprintln!("demo temp repo cleaned: {}", repo.display());
            write_report(&report, format.into(), out)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn build_report(repo: &Path, from: Option<&str>, to: Option<&str>) -> Result<SlopReport> {
    let config = SlopConfig::load(repo)?;
    eprintln!("loading git history...");
    let history = git_ingest::parse_range(repo, from, to)?;
    eprintln!("analyzing working tree...");
    let analysis = if from.is_some() || to.is_some() {
        let changed_paths = history
            .changes
            .iter()
            .map(|change| change.path.clone())
            .collect::<HashSet<_>>();
        analyze::analyze_repo_for_paths_with_config(repo, &history, &changed_paths, &config)?
    } else {
        analyze::analyze_repo_with_config(repo, &history, &config)?
    };
    eprintln!("attributing suspected AI commits...");
    let attributions = ai_attribution::attribute(&history);
    eprintln!("scoring findings...");
    let mut report = scoring::score(&history, analysis.findings, attributions);
    if from.is_some() || to.is_some() {
        apply_range_summary(&mut report, &history, range_label(from, to));
    }
    Ok(report)
}

fn range_label(from: Option<&str>, to: Option<&str>) -> String {
    match (from, to) {
        (Some(from), Some(to)) => format!("{from}..{to}"),
        (Some(from), None) => format!("{from}..HEAD"),
        (None, Some(to)) => to.to_string(),
        (None, None) => "full".into(),
    }
}

fn apply_range_summary(report: &mut SlopReport, history: &GitHistory, label: String) {
    let range_commits = history
        .commits
        .iter()
        .map(|commit| commit.oid.as_str())
        .collect::<HashSet<_>>();
    let ai_by_commit = report
        .attributions
        .iter()
        .map(|attr| (attr.commit_oid.as_str(), attr.ai_probability))
        .collect::<HashMap<_, _>>();
    let mut new_findings = 0;
    let mut ai_suspect_findings = 0;
    let mut debt_delta = 0.0;

    for finding in &mut report.findings {
        let is_new = finding
            .introduced_by
            .as_deref()
            .is_some_and(|oid| range_commits.contains(oid));
        if is_new {
            finding.status = Some(FindingStatus::New);
            new_findings += 1;
            debt_delta += report
                .finding_scores
                .get(&finding.fingerprint)
                .copied()
                .unwrap_or(0.0);
            if finding
                .introduced_by
                .as_deref()
                .and_then(|oid| ai_by_commit.get(oid).copied())
                .is_some_and(|probability| probability >= 0.3)
            {
                ai_suspect_findings += 1;
            }
        }
    }

    report.summary.new_findings = new_findings;
    report.summary.persistent_findings = report.findings.len().saturating_sub(new_findings);
    report.range_summary = Some(RangeSummary {
        label,
        new_findings,
        ai_suspect_findings,
        debt_delta: scoring::round2(debt_delta),
    });
}

fn write_report(
    slop_report: &slop_lens::ir::SlopReport,
    format: ReportFormat,
    out: Option<PathBuf>,
) -> Result<()> {
    let rendered = report::render(slop_report, format)?;
    let target = match (format, out) {
        (ReportFormat::Html, None) => Some(PathBuf::from("slop-lens-report.html")),
        (_, out) => out,
    };
    if let Some(path) = target {
        fs::write(&path, rendered)?;
        eprintln!("wrote report: {}", path.display());
    } else {
        print!("{rendered}");
    }
    Ok(())
}

fn gate_failed(report: &SlopReport, fail_on: Option<FailOn>, max_debt: Option<f64>) -> bool {
    fail_on.is_some_and(|level| findings_exceed_level(&report.findings, level))
        || max_debt.is_some_and(|threshold| report.summary.total_debt_score > threshold)
}

fn findings_exceed_level(findings: &[Finding], fail_on: FailOn) -> bool {
    findings.iter().any(|finding| match fail_on {
        FailOn::Error => finding.severity == 5,
        FailOn::Warning => finding.severity >= 3,
    })
}

fn parse_non_negative_f64(value: &str) -> Result<f64, String> {
    let parsed = value
        .parse::<f64>()
        .map_err(|_| "must be a number".to_string())?;
    if parsed.is_finite() && parsed >= 0.0 {
        Ok(parsed)
    } else {
        Err("must be a finite non-negative number".to_string())
    }
}

impl From<CliFormat> for ReportFormat {
    fn from(value: CliFormat) -> Self {
        match value {
            CliFormat::Terminal => Self::Terminal,
            CliFormat::Html => Self::Html,
            CliFormat::Json => Self::Json,
            CliFormat::Sarif => Self::Sarif,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slop_lens::ir::{Confidence, Summary};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn report_with(severity: u8, total_debt_score: f64) -> SlopReport {
        SlopReport {
            findings: vec![Finding {
                rule_id: "SL-001".into(),
                path: PathBuf::from("src/lib.rs"),
                line: 1,
                symbol_name: Some("dead".into()),
                severity,
                confidence: Confidence::High,
                evidence: "unused".into(),
                introduced_by: None,
                fingerprint: "fp".into(),
                status: None,
            }],
            attributions: vec![],
            file_scores: BTreeMap::new(),
            finding_scores: BTreeMap::new(),
            debt_timeline: vec![],
            summary: Summary {
                total_debt_score,
                finding_count: 1,
                ..Summary::default()
            },
            baseline_comparison: None,
            range_summary: None,
        }
    }

    #[test]
    fn gate_fails_on_error_severity_only_for_severity_five() {
        assert!(gate_failed(&report_with(5, 1.0), Some(FailOn::Error), None));
        assert!(!gate_failed(
            &report_with(4, 1.0),
            Some(FailOn::Error),
            None
        ));
    }

    #[test]
    fn gate_fails_when_debt_exceeds_threshold() {
        assert!(gate_failed(&report_with(3, 51.0), None, Some(50.0)));
        assert!(!gate_failed(&report_with(3, 50.0), None, Some(50.0)));
    }
}
