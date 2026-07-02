use crate::ir::{Attribution, Confidence, DebtTimelinePoint, Finding, FindingStatus, SlopReport};
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportFormat {
    Terminal,
    Html,
    Json,
    Sarif,
}

pub fn render(report: &SlopReport, format: ReportFormat) -> Result<String> {
    match format {
        ReportFormat::Terminal => Ok(render_terminal(report)),
        ReportFormat::Html => Ok(render_html(report)),
        ReportFormat::Json => Ok(serde_json::to_string_pretty(report)?),
        ReportFormat::Sarif => Ok(serde_json::to_string_pretty(&render_sarif_value(report))?),
    }
}

pub fn render_terminal(report: &SlopReport) -> String {
    let mut out = String::new();
    if let Some(range) = &report.range_summary {
        out.push_str(&format!("PR debt scan ({})\n", range.label));
        out.push_str(&format!(
            "本PR引入: {} new findings, {} AI-suspect, debt +{:.2}\n",
            range.new_findings, range.ai_suspect_findings, range.debt_delta
        ));
    } else {
        out.push_str("SlopLens debt summary\n");
    }
    if let Some(comparison) = &report.baseline_comparison {
        out.push_str(&format!(
            "baseline: debt {:.2}->{:.2} ({:+.0}%), 新增{}个finding, 修复{}个, 持续{}个\n",
            comparison.previous_debt_score,
            comparison.current_debt_score,
            comparison.percent_change,
            comparison.new_findings,
            comparison.resolved_findings,
            comparison.persistent_findings
        ));
    }
    out.push_str(&format!(
        "commits: {}  authors: {}  files: {}  findings: {}\n",
        report.summary.commit_count,
        report.summary.author_count,
        report.summary.file_count,
        report.summary.finding_count
    ));
    if report.summary.new_findings > 0
        || report.summary.resolved_findings > 0
        || report.summary.persistent_findings > 0
    {
        out.push_str(&format!(
            "finding diff: new {}  resolved {}  persistent {}\n",
            report.summary.new_findings,
            report.summary.resolved_findings,
            report.summary.persistent_findings
        ));
    }
    out.push_str(&format!(
        "debt score: {:.2}  AI-attributed debt: {:.2} ({:.0}%)\n\n",
        report.summary.total_debt_score,
        report.summary.ai_attributed_debt,
        report.summary.ai_attributed_ratio * 100.0
    ));
    out.push_str("score scale: 0-50 low, 50-150 medium, >150 high\n\n");
    out.push_str("Top debt files\n");
    for (path, score) in top_files(report, 10) {
        out.push_str(&format!("  {:>7.2}  {}\n", score, path.display()));
    }
    if report.file_scores.is_empty() {
        out.push_str("  no debt files found\n");
    }

    out.push_str("\nTop AI-suspect commits\n");
    for attr in top_attributions(report, 10) {
        let marker = if attr.ai_probability >= 0.8 && attr.evidence.len() >= 2 {
            "\x1b[1mHIGH\x1b[0m"
        } else {
            "suspected"
        };
        let short = attr.commit_oid.get(0..12).unwrap_or(&attr.commit_oid);
        out.push_str(&format!(
            "  {:>5.0}%  {marker:<9} {}  {} <{}>\n",
            attr.ai_probability * 100.0,
            short,
            attr.author.name,
            attr.author.email
        ));
        if !attr.evidence.is_empty() {
            out.push_str(&format!(
                "          evidence: {}\n",
                attr.evidence.join(", ")
            ));
        }
    }
    if report.attributions.is_empty() {
        out.push_str("  no AI-suspect commits found\n");
    }

    out.push_str("\nTop findings\n");
    for finding in top_findings(&report.findings, 10) {
        let tag = if finding.status == Some(FindingStatus::New) {
            "[NEW] "
        } else {
            ""
        };
        out.push_str(&format!(
            "  {}{} {} {} {}:{} {}\n",
            tag,
            finding.rule_id,
            color_severity(finding.severity),
            color_confidence(finding.confidence),
            finding.path.display(),
            finding.line,
            finding.evidence
        ));
    }
    if report.findings.is_empty() {
        out.push_str("  no findings found\n");
    }
    out
}

pub fn render_html(report: &SlopReport) -> String {
    let max_score = report
        .file_scores
        .values()
        .copied()
        .fold(0.0f64, f64::max)
        .max(1.0);
    let mut html = String::new();
    html.push_str(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>SlopLens Report</title>",
    );
    html.push_str("<style>body{font-family:system-ui,-apple-system,Segoe UI,sans-serif;margin:32px;color:#1f2937;background:#fafafa}h1,h2{margin:0 0 12px}section{margin:28px 0}.summary{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:12px}.metric{background:#fff;border:1px solid #ddd;border-radius:8px;padding:12px}.notice{background:#fff;border-left:4px solid #2563eb;padding:10px 12px;margin:12px 0}.scale{margin-top:12px}.heat{display:grid;grid-template-columns:repeat(auto-fill,minmax(220px,1fr));gap:8px}.tile{border:1px solid #d1d5db;border-radius:6px;padding:10px;background:#fff}.bar{height:8px;border-radius:4px;background:#d1d5db;margin-top:8px;overflow:hidden}.fill{height:100%}.timeline{width:100%;max-width:920px;background:#fff;border:1px solid #ddd}table{width:100%;border-collapse:collapse;background:#fff;border:1px solid #ddd}th,td{text-align:left;padding:8px;border-bottom:1px solid #eee;font-size:14px}th{background:#f3f4f6}.high{color:#b91c1c;font-weight:700}.tag{font-weight:700;color:#2563eb}.muted{color:#6b7280}</style>");
    let title = report
        .range_summary
        .as_ref()
        .map(|range| format!("PR debt scan ({})", range.label))
        .unwrap_or_else(|| "SlopLens Report".into());
    html.push_str(&format!("</head><body><h1>{}</h1>", escape(&title)));
    if let Some(range) = &report.range_summary {
        html.push_str(&format!(
            "<div class=\"notice\">本PR引入: {} new findings, {} AI-suspect, debt +{:.2}</div>",
            range.new_findings, range.ai_suspect_findings, range.debt_delta
        ));
    }
    if let Some(comparison) = &report.baseline_comparison {
        html.push_str(&format!(
            "<div class=\"notice\">baseline: debt {:.2}-&gt;{:.2} ({:+.0}%), 新增{}个finding, 修复{}个, 持续{}个</div>",
            comparison.previous_debt_score,
            comparison.current_debt_score,
            comparison.percent_change,
            comparison.new_findings,
            comparison.resolved_findings,
            comparison.persistent_findings
        ));
    }
    html.push_str("<section class=\"summary\">");
    for (label, value) in [
        ("Commits", report.summary.commit_count.to_string()),
        ("Authors", report.summary.author_count.to_string()),
        ("Files", report.summary.file_count.to_string()),
        ("Findings", report.summary.finding_count.to_string()),
        (
            "Debt Score",
            format!("{:.2}", report.summary.total_debt_score),
        ),
        (
            "AI-Attributed Debt",
            format!(
                "{:.2} ({:.0}%)",
                report.summary.ai_attributed_debt,
                report.summary.ai_attributed_ratio * 100.0
            ),
        ),
    ] {
        html.push_str(&format!(
            "<div class=\"metric\"><div class=\"muted\">{}</div><strong>{}</strong></div>",
            escape(label),
            escape(&value)
        ));
    }
    html.push_str("</section>");
    html.push_str(
        "<p class=\"muted scale\">Score scale: 0-50 low, 50-150 medium, &gt;150 high.</p>",
    );

    html.push_str("<section><h2>Debt Heatmap</h2><div class=\"heat\">");
    for (path, score) in top_files(report, 100) {
        let pct = ((score / max_score) * 100.0).clamp(4.0, 100.0);
        let color = heat_color(score / max_score);
        html.push_str(&format!(
            "<div class=\"tile\"><strong>{}</strong><div>{:.2}</div><div class=\"bar\"><div class=\"fill\" style=\"width:{:.1}%;background:{}\"></div></div></div>",
            escape(&path.display().to_string()),
            score,
            pct,
            color
        ));
    }
    html.push_str("</div></section>");

    html.push_str("<section><h2>AI Attribution Timeline</h2>");
    html.push_str(&render_timeline_svg(&report.debt_timeline));
    html.push_str("<table><thead><tr><th>Date</th><th>Commit</th><th>Probability</th><th>Author</th><th>Evidence</th></tr></thead><tbody>");
    for attr in top_attributions(report, 100) {
        let class = if attr.ai_probability >= 0.8 && attr.evidence.len() >= 2 {
            " class=\"high\""
        } else {
            ""
        };
        html.push_str(&format!(
            "<tr{}><td>{}</td><td>{}</td><td>{:.0}%</td><td>{} &lt;{}&gt;</td><td>{}</td></tr>",
            class,
            escape(&format_date(attr.commit_time)),
            escape(attr.commit_oid.get(0..12).unwrap_or(&attr.commit_oid)),
            attr.ai_probability * 100.0,
            escape(&attr.author.name),
            escape(&attr.author.email),
            escape(&attr.evidence.join("; "))
        ));
    }
    html.push_str("</tbody></table></section>");

    html.push_str("<section><h2>Findings</h2><table><thead><tr><th>Status</th><th>Rule</th><th>Severity</th><th>Confidence</th><th>Location</th><th>Evidence</th></tr></thead><tbody>");
    for finding in top_findings(&report.findings, report.findings.len()) {
        let status = if finding.status == Some(FindingStatus::New) {
            "<span class=\"tag\">[NEW]</span>"
        } else {
            ""
        };
        html.push_str(&format!(
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{:?}</td><td>{}:{}</td><td>{}</td></tr>",
            status,
            escape(&finding.rule_id),
            finding.severity,
            finding.confidence,
            escape(&finding.path.display().to_string()),
            finding.line,
            escape(&finding.evidence)
        ));
    }
    html.push_str("</tbody></table></section></body></html>");
    html
}

pub fn render_sarif_value(report: &SlopReport) -> Value {
    let rules = ["SL-001", "SL-002", "SL-003", "SL-004"]
        .into_iter()
        .map(|id| {
            json!({
                "id": id,
                "name": rule_name(id),
                "shortDescription": { "text": rule_name(id) },
                "helpUri": "https://github.com/slop-lens/slop-lens"
            })
        })
        .collect::<Vec<_>>();
    let results = report
        .findings
        .iter()
        .map(|finding| {
            json!({
                "ruleId": finding.rule_id,
                "level": sarif_level(finding),
                "message": { "text": finding.evidence },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": finding.path.to_string_lossy() },
                        "region": { "startLine": finding.line.max(1) }
                    }
                }],
                "fingerprints": {
                    "slopLens/v1": finding.fingerprint
                },
                "properties": {
                    "severity": finding.severity,
                    "confidence": format!("{:?}", finding.confidence),
                    "introducedBy": finding.introduced_by,
                    "status": finding.status.map(|status| format!("{status:?}"))
                }
            })
        })
        .collect::<Vec<_>>();
    json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "SlopLens",
                    "informationUri": "https://github.com/slop-lens/slop-lens",
                    "rules": rules
                }
            },
            "results": results
        }]
    })
}

fn top_files(report: &SlopReport, limit: usize) -> Vec<(PathBuf, f64)> {
    let mut files = report
        .file_scores
        .iter()
        .map(|(path, score)| (path.clone(), *score))
        .collect::<Vec<_>>();
    files.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal));
    files.truncate(limit);
    files
}

fn top_attributions(report: &SlopReport, limit: usize) -> Vec<&Attribution> {
    let mut attrs = report.attributions.iter().collect::<Vec<_>>();
    attrs.sort_by(|a, b| {
        b.ai_probability
            .partial_cmp(&a.ai_probability)
            .unwrap_or(Ordering::Equal)
    });
    attrs.truncate(limit);
    attrs
}

fn top_findings(findings: &[Finding], limit: usize) -> Vec<&Finding> {
    let mut sorted = findings.iter().collect::<Vec<_>>();
    sorted.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
    sorted.truncate(limit);
    sorted
}

fn rule_name(rule_id: &str) -> &'static str {
    match rule_id {
        "SL-001" => "Unused candidate",
        "SL-002" => "Duplicate block",
        "SL-003" => "Complexity candidate",
        "SL-004" => "Comment inflation candidate",
        _ => "SlopLens finding",
    }
}

fn sarif_level(finding: &Finding) -> &'static str {
    if finding.rule_id == "SL-003" {
        return "warning";
    }
    match finding.severity {
        5 => "error",
        3 | 4 => "warning",
        _ => "note",
    }
}

fn color_severity(severity: u8) -> String {
    let label = format!("sev{severity}");
    match severity {
        4 | 5 => format!("\x1b[31m{label}\x1b[0m"),
        3 => format!("\x1b[33m{label}\x1b[0m"),
        _ => format!("\x1b[90m{label}\x1b[0m"),
    }
}

fn color_confidence(confidence: Confidence) -> String {
    match confidence {
        Confidence::High => "\x1b[1mHigh\x1b[0m".into(),
        Confidence::Medium => "Medium".into(),
        Confidence::Low => "\x1b[90mLow\x1b[0m".into(),
    }
}

fn heat_color(ratio: f64) -> String {
    let ratio = ratio.clamp(0.0, 1.0);
    let red = (37.0 + (220.0 - 37.0) * ratio).round() as u8;
    let green = (99.0 + (38.0 - 99.0) * ratio).round() as u8;
    let blue = (235.0 + (38.0 - 235.0) * ratio).round() as u8;
    format!("rgb({red},{green},{blue})")
}

fn render_timeline_svg(points: &[DebtTimelinePoint]) -> String {
    let width = 920.0;
    let height = 220.0;
    let pad = 32.0;
    if points.is_empty() {
        return "<svg class=\"timeline\" viewBox=\"0 0 920 220\" role=\"img\" aria-label=\"Debt timeline\"><line x1=\"32\" y1=\"188\" x2=\"888\" y2=\"188\" stroke=\"#d1d5db\"/><text x=\"32\" y=\"112\" fill=\"#6b7280\">No debt timeline data</text></svg>".into();
    }

    let min_time = points
        .iter()
        .map(|point| point.commit_time)
        .min()
        .unwrap_or_default();
    let max_time = points
        .iter()
        .map(|point| point.commit_time)
        .max()
        .unwrap_or(min_time);
    let max_debt = points
        .iter()
        .map(|point| point.cumulative_debt_score)
        .fold(0.0f64, f64::max)
        .max(1.0);
    let x = |time: i64| {
        if max_time == min_time {
            width / 2.0
        } else {
            pad + ((time - min_time) as f64 / (max_time - min_time) as f64) * (width - pad * 2.0)
        }
    };
    let y = |debt: f64| height - pad - (debt / max_debt) * (height - pad * 2.0);
    let polyline = points
        .iter()
        .map(|point| {
            format!(
                "{:.1},{:.1}",
                x(point.commit_time),
                y(point.cumulative_debt_score)
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mut svg = format!(
        "<svg class=\"timeline\" viewBox=\"0 0 920 220\" role=\"img\" aria-label=\"Debt cumulative timeline\"><line x1=\"{pad}\" y1=\"{}\" x2=\"{}\" y2=\"{}\" stroke=\"#d1d5db\"/><line x1=\"{pad}\" y1=\"{pad}\" x2=\"{pad}\" y2=\"{}\" stroke=\"#d1d5db\"/><polyline points=\"{}\" fill=\"none\" stroke=\"#2563eb\" stroke-width=\"3\"/>",
        height - pad,
        width - pad,
        height - pad,
        height - pad,
        polyline
    );
    for point in points {
        let color = if point.ai_probability >= 0.3 {
            "#dc2626"
        } else {
            "#2563eb"
        };
        svg.push_str(&format!(
            "<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"4\" fill=\"{}\"><title>{}: {:.2}</title></circle>",
            x(point.commit_time),
            y(point.cumulative_debt_score),
            color,
            escape(point.commit_oid.get(0..12).unwrap_or(&point.commit_oid)),
            point.cumulative_debt_score
        ));
    }
    svg.push_str(&format!(
        "<text x=\"{pad}\" y=\"20\" fill=\"#6b7280\">Debt {:.2}</text><text x=\"{}\" y=\"208\" fill=\"#6b7280\" text-anchor=\"end\">AI commits are red</text></svg>",
        max_debt,
        width - pad
    ));
    svg
}

fn format_date(timestamp: i64) -> String {
    DateTime::<Utc>::from_timestamp(timestamp, 0)
        .map(|date| date.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Confidence, Finding, SlopReport, Summary};
    use std::collections::BTreeMap;

    fn sample_report() -> SlopReport {
        SlopReport {
            findings: vec![Finding {
                rule_id: "SL-001".into(),
                path: PathBuf::from("src/lib.rs"),
                line: 2,
                symbol_name: Some("unused".into()),
                severity: 3,
                confidence: Confidence::High,
                evidence: "unused candidate".into(),
                introduced_by: Some("abc".into()),
                fingerprint: "fp".into(),
                status: None,
            }],
            attributions: vec![],
            file_scores: BTreeMap::from([(PathBuf::from("src/lib.rs"), 3.0)]),
            finding_scores: BTreeMap::from([("fp".into(), 3.0)]),
            debt_timeline: vec![],
            summary: Summary {
                commit_count: 1,
                author_count: 1,
                file_count: 1,
                finding_count: 1,
                total_debt_score: 3.0,
                ai_attributed_debt: 0.0,
                ai_attributed_ratio: 0.0,
                ..Summary::default()
            },
            baseline_comparison: None,
            range_summary: None,
        }
    }

    #[test]
    fn json_report_is_valid_json() {
        let json = render(&sample_report(), ReportFormat::Json).unwrap();
        let parsed: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["summary"]["finding_count"], 1);
    }

    #[test]
    fn sarif_report_has_required_shape() {
        let sarif = render_sarif_value(&sample_report());
        assert_eq!(sarif["version"], "2.1.0");
        assert_eq!(sarif["runs"][0]["results"][0]["ruleId"], "SL-001");
        assert_eq!(
            sarif["runs"][0]["results"][0]["fingerprints"]["slopLens/v1"],
            "fp"
        );
        assert!(sarif["runs"][0]["results"][0]["partialFingerprints"].is_null());
    }

    #[test]
    fn complexity_severity_five_stays_sarif_warning() {
        let mut report = sample_report();
        report.findings[0].rule_id = "SL-003".into();
        report.findings[0].severity = 5;
        let sarif = render_sarif_value(&report);
        assert_eq!(sarif["runs"][0]["results"][0]["level"], "warning");
    }
}
