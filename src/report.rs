use crate::ir::{Attribution, Confidence, Finding, FindingStatus, SlopReport};
use anyhow::Result;
use chrono::{DateTime, Datelike, Utc};
use serde_json::{Value, json};
use std::cmp::Ordering;
use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportFormat {
    Terminal,
    Summary,
    Html,
    Json,
    Sarif,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportFooter {
    pub version: String,
    pub date: String,
    pub head: String,
}

pub fn render(report: &SlopReport, format: ReportFormat) -> Result<String> {
    match format {
        ReportFormat::Terminal => Ok(render_terminal(report)),
        ReportFormat::Summary => Ok(render_summary(report, None)),
        ReportFormat::Html => Ok(render_html(report)),
        ReportFormat::Json => Ok(serde_json::to_string_pretty(report)?),
        ReportFormat::Sarif => Ok(serde_json::to_string_pretty(&render_sarif_value(report))?),
    }
}

pub fn render_terminal(report: &SlopReport) -> String {
    render_terminal_with_color(report, io::stdout().is_terminal())
}

pub fn render_terminal_with_color(report: &SlopReport, use_color: bool) -> String {
    let mut out = String::new();
    if let Some(range) = &report.range_summary {
        out.push_str(&format!("PR debt scan ({})\n", range.label));
        out.push_str(&format!(
            "{} new findings, {} AI-suspect findings, debt +{:.2}\n",
            range.new_findings, range.ai_suspect_findings, range.debt_delta
        ));
    } else {
        out.push_str("SlopLens debt summary\n");
    }
    if let Some(comparison) = &report.baseline_comparison {
        out.push_str(&format!(
            "baseline: debt {:.2}->{:.2} ({:+.0}%), {} new findings, {} resolved, {} persistent\n",
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
        "debt index: {:.2}/100 ({})\nAI-attributed debt: {:.2} ({:.0}%)\n\n",
        report.summary.debt_index,
        debt_level(report.summary.debt_index),
        report.summary.ai_attributed_debt,
        report.summary.ai_attributed_ratio * 100.0
    ));
    out.push_str("index scale: 0-40 low, 40-70 medium, >70 high\n\n");
    out.push_str("Top debt files\n");
    for (path, score) in top_files(report, 10) {
        out.push_str(&format!("  {:>7.2}  {}\n", score, path.display()));
    }
    if report.file_scores.is_empty() {
        out.push_str("  no debt files found\n");
    }

    out.push_str("\nTop AI-suspect commits\n");
    for attr in top_attributions(report, 10) {
        let marker = if attr.ai_probability >= 0.8 {
            colorize("HIGH", "\x1b[1m", use_color).into_owned()
        } else {
            "suspected".into()
        };
        let short = attr.commit_oid.get(0..12).unwrap_or(&attr.commit_oid);
        let message = commit_subject(commit_message(report, &attr.commit_oid));
        out.push_str(&format!(
            "  {:>5.0}%  {marker:<9} {}  {}  {} <{}>\n",
            attr.ai_probability * 100.0,
            short,
            truncate_end(message, 40),
            attr.author.name,
            attr.author.email
        ));
        if !attr.evidence.is_empty() {
            out.push_str(&format!("          evidence: {}\n", evidence_summary(attr)));
        }
    }
    if report.attributions.is_empty() {
        out.push_str("  no AI-suspect commits found\n");
    }

    out.push_str("\nTop findings\n");
    for finding in top_findings(report, 10) {
        let tag = if finding.status == Some(FindingStatus::New) {
            "[NEW] "
        } else {
            ""
        };
        let introduced = introduced_terminal(report, finding);
        out.push_str(&format!(
            "  {}{} {} {} {}:{}  {}  {}\n",
            tag,
            finding.rule_id,
            color_severity(finding.severity, use_color),
            color_confidence(finding.confidence, use_color),
            finding.path.display(),
            finding.line,
            introduced,
            finding.evidence
        ));
    }
    if report.findings.is_empty() {
        out.push_str("  no findings found\n");
    }
    out
}

pub fn render_summary(report: &SlopReport, elapsed_seconds: Option<f64>) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "debt index: {:.2}/100 ({}) · {} findings · AI debt: {:.0}%\n",
        report.summary.debt_index,
        debt_level(report.summary.debt_index),
        report.summary.finding_count,
        report.summary.ai_attributed_ratio * 100.0
    ));
    if let Some((path, score)) = top_files(report, 1).into_iter().next() {
        out.push_str(&format!("top: {} ({:.2})\n", path.display(), score));
    } else {
        out.push_str("top: none\n");
    }
    out.push_str(&format!(
        "scope: {} commits · {} files · {} authors\n",
        report.summary.commit_count, report.summary.file_count, report.summary.author_count
    ));
    if let Some(seconds) = elapsed_seconds {
        out.push_str(&format!("scan complete in {:.1}s\n", seconds));
    }
    out
}

pub fn render_html(report: &SlopReport) -> String {
    render_html_with_footer(report, None)
}

pub fn render_html_with_footer(report: &SlopReport, footer: Option<&ReportFooter>) -> String {
    let mut html = String::new();
    html.push_str(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>SlopLens Report</title>",
    );
    html.push_str("<style>:root{--bg:#16181d;--card:#1e2128;--text:#c8ccd4;--good:#7FB069;--warn:#D8A657;--bad:#D66A61;--ai:#9B7EDE;--human:#4FA3C7;--accent:#2DD4BF;--code:#B8C7D9;--line:#303640;--muted:#8d96a6}*{box-sizing:border-box}body{font-family:Inter,ui-sans-serif,system-ui,-apple-system,Segoe UI,sans-serif;margin:0;color:var(--text);background:var(--bg);line-height:1.45}main{max-width:1180px;margin:0 auto;padding:32px 24px 32px}footer{max-width:1180px;margin:0 auto;padding:0 24px 32px;color:var(--muted);font-size:12px}a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}h1,h2{margin:0;color:#eef1f5;font-weight:700}h1{font-size:22px}h2{font-size:16px;letter-spacing:.02em;text-transform:uppercase}section{margin:26px 0}.verdict,.panel{background:var(--card);border:1px solid var(--line);border-radius:8px}.verdict{padding:26px 28px}.verdict-top{display:flex;justify-content:space-between;gap:18px;align-items:flex-start}.index{font-size:38px;line-height:1.1;font-weight:750;color:#f3f5f8}.level{font-size:14px;padding:3px 8px;border:1px solid currentColor;border-radius:4px;margin-left:8px}.level.high,.trend.up{color:var(--bad)}.level.medium{color:var(--warn)}.level.low,.trend.down{color:var(--good)}.subtitle{margin:12px 0 0;color:var(--text);font-size:15px}.kpis{display:grid;grid-template-columns:repeat(auto-fit,minmax(140px,1fr));gap:1px;background:var(--line);border:1px solid var(--line);border-radius:8px;overflow:hidden;margin-top:20px}.kpi{background:#1a1d23;padding:12px}.kpi span,.muted{display:block;color:var(--muted);font-size:12px}.kpi strong{display:block;margin-top:4px;color:#eef1f5;font-size:17px}.notice{border-left:3px solid var(--accent);padding:10px 12px;margin:14px 0;background:#1a1d23;color:var(--code)}.viz{padding:16px}.treemap,.timeline{display:block;width:100%;background:#191c22;border:1px solid var(--line);border-radius:6px}.treemap text{pointer-events:none}.axis{stroke:#566071;stroke-width:1}.grid{stroke:#2c323c;stroke-width:1}.table-tools{display:flex;flex-wrap:wrap;gap:10px;align-items:center;margin:12px 0}.table-tools input{min-width:260px;flex:1 1 260px;background:#191c22;border:1px solid var(--line);border-radius:6px;color:var(--text);padding:8px 10px}.rule-filters{display:flex;flex-wrap:wrap;gap:6px}.rule-filters button{background:#191c22;border:1px solid var(--line);border-radius:6px;color:var(--text);padding:7px 9px;cursor:pointer}.rule-filters button.active{border-color:var(--accent);color:var(--accent)}.table-wrap{overflow-x:auto;border:1px solid var(--line);border-radius:8px;background:var(--card)}table{width:100%;border-collapse:collapse;min-width:980px}th,td{text-align:left;padding:10px 12px;border-bottom:1px solid var(--line);font-size:13px;vertical-align:top}th{color:#eef1f5;background:#191c22;font-size:12px;text-transform:uppercase;letter-spacing:.04em}th.sortable{cursor:pointer;user-select:none}tr:last-child td{border-bottom:0}.code{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;color:var(--code)}.priority{font-weight:700;color:#eef1f5}.tag,.prob{display:inline-block;border:1px solid var(--line);border-radius:4px;padding:2px 6px;color:var(--muted);font-size:12px}.tag.ai{border-color:rgba(155,126,222,.55);color:var(--ai)}.tag.new{border-color:rgba(45,212,191,.55);color:var(--accent)}.tag.human{border-color:rgba(79,163,199,.55);color:var(--human)}.prob.high{border-color:rgba(214,106,97,.65);color:var(--bad)}.prob.likely{border-color:rgba(216,166,87,.7);color:var(--warn)}.prob.possible{border-color:rgba(141,150,166,.75);color:var(--muted)}details summary{cursor:pointer;color:#eef1f5}details div{margin-top:8px;color:var(--text)}.fix{color:var(--accent)}.small{font-size:12px}</style>");
    let title = report
        .range_summary
        .as_ref()
        .map(|range| format!("PR debt scan ({})", range.label))
        .unwrap_or_else(|| "SlopLens Report".into());
    html.push_str(&format!("</head><body><main><h1>{}</h1>", escape(&title)));
    if let Some(range) = &report.range_summary {
        html.push_str(&format!(
            "<div class=\"notice\">{} new findings, {} AI-suspect findings, debt +{:.2}</div>",
            range.new_findings, range.ai_suspect_findings, range.debt_delta
        ));
    }
    if let Some(comparison) = &report.baseline_comparison {
        html.push_str(&format!(
            "<div class=\"notice\">baseline: debt {:.2}-&gt;{:.2} ({:+.0}%), {} new findings, {} resolved, {} persistent</div>",
            comparison.previous_debt_score,
            comparison.current_debt_score,
            comparison.percent_change,
            comparison.new_findings,
            comparison.resolved_findings,
            comparison.persistent_findings
        ));
    }
    let level = debt_level(report.summary.debt_index);
    html.push_str("<section class=\"verdict\">");
    html.push_str("<div class=\"verdict-top\"><div>");
    html.push_str(&format!(
        "<div class=\"index\">Debt Index: {:.2}/100 <span class=\"level {}\">{}</span></div>",
        report.summary.debt_index,
        level.to_ascii_lowercase(),
        level
    ));
    html.push_str(&format!(
        "<p class=\"subtitle\">{:.0}% of debt attributed to suspected AI commits</p>",
        report.summary.ai_attributed_ratio * 100.0
    ));
    html.push_str(&format!(
        "<p class=\"muted\">Debt Index = debt score / total LOC * 1000, clamped to 0-100. Scale: 0-40 LOW, 40-70 MEDIUM, >70 HIGH. Current debt/LOC: {:.4}</p>",
        debt_per_loc(report)
    ));
    html.push_str("</div>");
    if report.baseline_comparison.is_some() {
        let previous_index = previous_debt_index(report);
        let trend_class = if report.summary.debt_index >= previous_index {
            "up"
        } else {
            "down"
        };
        let arrow = if report.summary.debt_index >= previous_index {
            "&#8593;"
        } else {
            "&#8595;"
        };
        html.push_str(&format!(
            "<div class=\"trend {}\"><span class=\"muted\">Baseline trend</span><strong>{:.0}&rarr;{:.0} {}</strong></div>",
            trend_class, previous_index, report.summary.debt_index, arrow
        ));
    }
    html.push_str("</div><div class=\"kpis\">");
    for (label, value) in [
        ("Commits", report.summary.commit_count.to_string()),
        ("Authors", report.summary.author_count.to_string()),
        ("Files", report.summary.file_count.to_string()),
        ("Findings", report.summary.finding_count.to_string()),
        (
            "Debt Score",
            format!("{:.2}", report.summary.total_debt_score),
        ),
        ("Total LOC", report.summary.total_loc.to_string()),
    ] {
        html.push_str(&format!(
            "<div class=\"kpi\"><span>{}</span><strong>{}</strong></div>",
            escape(label),
            escape(&value)
        ));
    }
    html.push_str("</div></section>");

    html.push_str("<section class=\"panel\"><div class=\"viz\"><h2>File Debt Treemap</h2>");
    html.push_str(&render_treemap_svg(report));
    html.push_str("</div></section>");

    html.push_str("<section class=\"panel\"><div class=\"viz\"><h2>Debt Timeline</h2>");
    html.push_str(&render_timeline_svg(report));
    html.push_str("</div></section>");

    html.push_str("<section><h2>AI Attribution Timeline</h2><div class=\"table-wrap\"><table><thead><tr><th>Date</th><th>Commit</th><th>Message</th><th>Probability</th><th>Evidence</th><th>Author</th></tr></thead><tbody>");
    for attr in top_attributions(report, 100) {
        if ai_signal(attr).is_none() {
            continue;
        }
        let message = commit_subject(commit_message(report, &attr.commit_oid));
        html.push_str(&format!(
            "<tr><td>{}</td><td class=\"code\">{}</td><td>{}</td><td>{}</td><td>{}</td><td>{} &lt;{}&gt;</td></tr>",
            escape(&format_date(attr.commit_time)),
            commit_link_html(report, &attr.commit_oid),
            escape(&truncate_end(message, 40)),
            probability_badge_html(attr),
            escape(&evidence_summary(attr)),
            escape(&attr.author.name),
            escape(&attr.author.email),
        ));
    }
    html.push_str("</tbody></table></div></section>");

    html.push_str("<section><h2>Actionable Findings</h2><div class=\"table-tools\"><input id=\"findings-search\" type=\"search\" placeholder=\"Search by file or rule\" aria-label=\"Search findings by file or rule\"><div class=\"rule-filters\" aria-label=\"Filter findings by rule\"><button type=\"button\" class=\"active\" data-rule-filter=\"all\">All</button><button type=\"button\" data-rule-filter=\"SL-001\">SL-001</button><button type=\"button\" data-rule-filter=\"SL-002\">SL-002</button><button type=\"button\" data-rule-filter=\"SL-003\">SL-003</button><button type=\"button\" data-rule-filter=\"SL-004\">SL-004</button></div></div><div class=\"table-wrap\"><table id=\"findings-table\"><thead><tr><th>Priority</th><th>Rule</th><th class=\"sortable\" data-sort=\"severity\">Severity</th><th>Location</th><th>Introduced By</th><th>AI</th><th>Action</th></tr></thead><tbody>");
    for finding in top_findings(report, report.findings.len()) {
        let status = if finding.status == Some(FindingStatus::New) {
            "<span class=\"tag new\">NEW</span> "
        } else {
            ""
        };
        let ai = ai_for_finding(report, finding)
            .and_then(ai_signal)
            .map(|signal| format!("<span class=\"tag ai\">{}</span>", escape(signal)))
            .unwrap_or_else(|| "<span class=\"tag human\">human</span>".into());
        html.push_str(&format!(
            "<tr data-rule=\"{}\" data-file=\"{}\" data-severity=\"{}\"><td class=\"priority\">{:.2}</td><td>{}{}</td><td>sev{}<div class=\"muted\">{:?}</div></td><td>{}</td><td>{}</td><td>{}</td><td><details><summary>{}</summary><div><strong>Evidence:</strong> {}</div><div class=\"fix\"><strong>Suggested fix:</strong> {}</div></details></td></tr>",
            escape(&finding.rule_id),
            escape(&finding.path.display().to_string()),
            finding.severity,
            finding_priority(report, finding),
            status,
            escape(&finding.rule_id),
            finding.severity,
            finding.confidence,
            finding_location_html(report, finding),
            introduced_html(report, finding),
            ai,
            escape(rule_name(&finding.rule_id)),
            escape(&finding.evidence),
            escape(fix_suggestion(&finding.rule_id))
        ));
    }
    html.push_str("</tbody></table></div></section></main>");
    if let Some(footer) = footer {
        html.push_str(&format!(
            "<footer>Generated by SlopLens v{} · {} · HEAD={}</footer>",
            escape(&footer.version),
            escape(&footer.date),
            escape(&footer.head)
        ));
    }
    html.push_str("<script>(function(){const input=document.getElementById('findings-search');const table=document.getElementById('findings-table');if(!input||!table)return;const tbody=table.tBodies[0];const buttons=[...document.querySelectorAll('[data-rule-filter]')];let rule='all';let severityDir=-1;function apply(){const q=input.value.trim().toLowerCase();[...tbody.rows].forEach(row=>{const matchesRule=rule==='all'||row.dataset.rule===rule;const hay=(row.dataset.file+' '+row.dataset.rule).toLowerCase();row.hidden=!(matchesRule&&hay.includes(q));});}input.addEventListener('input',apply);buttons.forEach(button=>button.addEventListener('click',()=>{rule=button.dataset.ruleFilter;buttons.forEach(item=>item.classList.toggle('active',item===button));apply();}));const severityHeader=table.querySelector('[data-sort=\"severity\"]');if(severityHeader){severityHeader.addEventListener('click',()=>{const rows=[...tbody.rows];rows.sort((a,b)=>severityDir*(Number(a.dataset.severity)-Number(b.dataset.severity)));severityDir*=-1;rows.forEach(row=>tbody.appendChild(row));apply();});}}());</script></body></html>");
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

fn top_findings(report: &SlopReport, limit: usize) -> Vec<&Finding> {
    let mut sorted = report.findings.iter().collect::<Vec<_>>();
    sorted.sort_by(|a, b| {
        finding_priority(report, b)
            .partial_cmp(&finding_priority(report, a))
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
    sorted.truncate(limit);
    sorted
}

fn finding_priority(report: &SlopReport, finding: &Finding) -> f64 {
    report
        .finding_scores
        .get(&finding.fingerprint)
        .copied()
        .unwrap_or_else(|| f64::from(finding.severity) * finding.confidence.weight())
}

fn debt_level(index: f64) -> &'static str {
    if index >= 70.0 {
        "HIGH"
    } else if index >= 40.0 {
        "MEDIUM"
    } else {
        "LOW"
    }
}

fn debt_per_loc(report: &SlopReport) -> f64 {
    if report.summary.total_loc == 0 {
        0.0
    } else {
        report.summary.total_debt_score / report.summary.total_loc as f64
    }
}

fn previous_debt_index(report: &SlopReport) -> f64 {
    let Some(comparison) = &report.baseline_comparison else {
        return report.summary.debt_index;
    };
    if comparison.current_debt_score <= 0.0 {
        return 0.0;
    }
    (report.summary.debt_index * comparison.previous_debt_score / comparison.current_debt_score)
        .clamp(0.0, 100.0)
}

fn finding_location_html(report: &SlopReport, finding: &Finding) -> String {
    let location = format!("{}:{}", finding.path.display(), finding.line);
    let linked = github_file_url(report, &finding.path, finding.line)
        .map(|url| {
            format!(
                "<a href=\"{}\" target=\"_blank\" rel=\"noopener\">{}</a>",
                escape(&url),
                escape(&location)
            )
        })
        .unwrap_or_else(|| escape(&location));
    match &finding.symbol_name {
        Some(symbol) => format!("{linked}<div class=\"muted\">{}</div>", escape(symbol)),
        None => linked,
    }
}

fn introduced_terminal(report: &SlopReport, finding: &Finding) -> String {
    let Some(oid) = finding.introduced_by.as_deref() else {
        return "introduced_by=unknown".into();
    };
    let message = truncate_end(commit_subject(commit_message(report, oid)), 40);
    format!("introduced_by={} {}", short_oid(oid), message)
}

fn introduced_html(report: &SlopReport, finding: &Finding) -> String {
    let Some(oid) = finding.introduced_by.as_deref() else {
        return "<span class=\"muted\">unknown</span>".into();
    };
    let message = truncate_end(commit_subject(commit_message(report, oid)), 40);
    format!(
        "{}<div class=\"muted\">{}</div>",
        commit_link_html(report, oid),
        escape(&message)
    )
}

fn commit_message<'a>(report: &'a SlopReport, oid: &str) -> &'a str {
    report
        .commit_messages
        .get(oid)
        .map(String::as_str)
        .unwrap_or("")
}

fn commit_subject(message: &str) -> &str {
    message
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(str::trim)
        .unwrap_or("")
}

fn commit_link_html(report: &SlopReport, oid: &str) -> String {
    let label = escape(short_oid(oid));
    github_commit_url(report, oid)
        .map(|url| {
            format!(
                "<a href=\"{}\" target=\"_blank\" rel=\"noopener\">{label}</a>",
                escape(&url)
            )
        })
        .unwrap_or(label)
}

fn probability_badge_html(attr: &Attribution) -> String {
    let (class, label) = if attr.ai_probability >= 0.8 {
        ("high", "high")
    } else if attr.ai_probability >= 0.5 {
        ("likely", "likely")
    } else {
        ("possible", "possible")
    };
    format!(
        "<span class=\"prob {class}\">{:.0}% {label}</span>",
        attr.ai_probability * 100.0
    )
}

fn github_commit_url(report: &SlopReport, oid: &str) -> Option<String> {
    let repo_url = report.repo_url.as_deref()?.trim_end_matches('/');
    Some(format!("{repo_url}/commit/{oid}"))
}

fn github_file_url(report: &SlopReport, path: &Path, line: usize) -> Option<String> {
    let repo_url = report.repo_url.as_deref()?.trim_end_matches('/');
    let path = encode_url_path(&path.to_string_lossy());
    Some(format!("{repo_url}/blob/HEAD/{path}#L{}", line.max(1)))
}

fn short_oid(oid: &str) -> &str {
    oid.get(0..12).unwrap_or(oid)
}

fn truncate_end(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.into();
    }
    let keep = max_chars.saturating_sub(3);
    let prefix = value.chars().take(keep).collect::<String>();
    format!("{prefix}...")
}

fn encode_url_path(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn fix_suggestion(rule_id: &str) -> &'static str {
    match rule_id {
        "SL-001" => "Remove the unused code or add a real call site",
        "SL-002" => "Extract a shared helper",
        "SL-003" => "Split the deepest nesting first",
        "SL-004" => "Remove stale comments or replace them with useful intent",
        _ => "Inspect the finding and reduce the debt at the reported location",
    }
}

fn ai_for_finding<'a>(report: &'a SlopReport, finding: &Finding) -> Option<&'a Attribution> {
    let oid = finding.introduced_by.as_deref()?;
    report
        .attributions
        .iter()
        .find(|attr| attr.commit_oid == oid)
}

fn ai_signal(attr: &Attribution) -> Option<&'static str> {
    if attr.ai_probability >= 0.8 {
        Some("confirmed signal")
    } else if attr.ai_probability >= 0.5 {
        Some("likely")
    } else {
        None
    }
}

fn evidence_summary(attr: &Attribution) -> String {
    let mut evidence = attr
        .evidence
        .iter()
        .map(|evidence| human_evidence(evidence))
        .collect::<Vec<_>>();
    evidence.sort();
    evidence.dedup();
    if evidence.is_empty() {
        "No evidence details".into()
    } else {
        evidence.join(" · ")
    }
}

fn human_evidence(raw: &str) -> String {
    let (without_score, confidence) = evidence_score(raw)
        .map(|(prefix, score)| (prefix, confidence_label(score)))
        .unwrap_or((raw, "low"));
    let (kind, detail) = without_score
        .split_once(':')
        .map(|(kind, detail)| (kind.trim(), detail.trim()))
        .unwrap_or(("signal", without_score.trim()));
    let lower_detail = detail.to_ascii_lowercase();
    let phrase = match kind {
        "trailer" if lower_detail.contains("copilot") && lower_detail.contains("co-authored") => {
            "Copilot co-author trailer detected".into()
        }
        "trailer" => format!(
            "AI-related commit trailer detected: {}",
            clean_evidence_detail(detail)
        ),
        "metadata" => "AI or GitHub web authoring metadata detected".into(),
        "churn" => "Large low-coordination diff detected".into(),
        "burst" => "Large multi-file commit landed close to same-author work".into(),
        "shape" if lower_detail.contains("even churn") => "Even additive churn across files".into(),
        "shape" if lower_detail.contains("test/documentation") => {
            "High test/documentation file ratio".into()
        }
        "shape" => title_case_detail(detail),
        "style" => "Author style/churn shifted sharply versus prior commits".into(),
        "msg" => "Template-like commit message detected".into(),
        _ => title_case_detail(detail),
    };
    format!("{phrase} ({confidence} confidence)")
}

fn evidence_score(raw: &str) -> Option<(&str, f64)> {
    let marker = raw.rfind("(+")?;
    let end = raw[marker + 2..].find(')')? + marker + 2;
    let score = raw[marker + 2..end].parse::<f64>().ok()?;
    Some((raw[..marker].trim(), score))
}

fn confidence_label(score: f64) -> &'static str {
    if score >= 0.8 {
        "high"
    } else if score >= 0.5 {
        "medium"
    } else {
        "low"
    }
}

fn clean_evidence_detail(detail: &str) -> String {
    detail.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn title_case_detail(detail: &str) -> String {
    let detail = clean_evidence_detail(detail);
    let mut chars = detail.chars();
    let Some(first) = chars.next() else {
        return "AI signal detected".into();
    };
    format!("{}{}", first.to_uppercase(), chars.collect::<String>())
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

fn color_severity(severity: u8, use_color: bool) -> String {
    let label = format!("sev{severity}");
    match severity {
        4 | 5 => colorize(&label, "\x1b[31m", use_color).into_owned(),
        3 => colorize(&label, "\x1b[33m", use_color).into_owned(),
        _ => colorize(&label, "\x1b[90m", use_color).into_owned(),
    }
}

fn color_confidence(confidence: Confidence, use_color: bool) -> String {
    match confidence {
        Confidence::High => colorize("High", "\x1b[1m", use_color).into_owned(),
        Confidence::Medium => "Medium".into(),
        Confidence::Low => colorize("Low", "\x1b[90m", use_color).into_owned(),
    }
}

fn colorize<'a>(label: &'a str, prefix: &str, use_color: bool) -> std::borrow::Cow<'a, str> {
    if use_color {
        format!("{prefix}{label}\x1b[0m").into()
    } else {
        label.into()
    }
}

fn heat_color(ratio: f64) -> String {
    let ratio = ratio.clamp(0.0, 1.0);
    let (start, end, t) = if ratio <= 0.5 {
        ((127.0, 176.0, 105.0), (216.0, 166.0, 87.0), ratio * 2.0)
    } else {
        (
            (216.0, 166.0, 87.0),
            (214.0, 106.0, 97.0),
            (ratio - 0.5) * 2.0,
        )
    };
    let red = (start.0 + (end.0 - start.0) * t).round() as u8;
    let green = (start.1 + (end.1 - start.1) * t).round() as u8;
    let blue = (start.2 + (end.2 - start.2) * t).round() as u8;
    format!("rgb({red},{green},{blue})")
}

#[derive(Debug, Clone)]
struct TreemapItem {
    path: String,
    path_url: Option<String>,
    score: f64,
    top_finding: String,
    area: f64,
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn render_treemap_svg(report: &SlopReport) -> String {
    let width = 1120.0;
    let height = 420.0;
    if report.file_scores.is_empty() {
        return "<svg class=\"treemap\" data-chart=\"treemap\" viewBox=\"0 0 1120 420\" role=\"img\" aria-label=\"File debt treemap\"><text x=\"24\" y=\"44\" fill=\"#8d96a6\">No file debt found</text></svg>".into();
    }

    let total = report.file_scores.values().sum::<f64>().max(1.0);
    let max_score = report
        .file_scores
        .values()
        .copied()
        .fold(0.0f64, f64::max)
        .max(1.0);
    let mut items = report
        .file_scores
        .iter()
        .map(|(path, score)| {
            let top_finding = top_file_finding(report, path);
            TreemapItem {
                path: path.display().to_string(),
                path_url: github_file_url(
                    report,
                    path,
                    top_finding
                        .as_ref()
                        .map(|finding| finding.line)
                        .unwrap_or(1),
                ),
                score: *score,
                top_finding: top_finding
                    .map(|finding| finding.summary)
                    .unwrap_or_else(|| "No finding summary".into()),
                area: (*score / total) * width * height,
            }
        })
        .collect::<Vec<_>>();
    items.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));

    let rects = squarify(
        items,
        Rect {
            x: 0.0,
            y: 0.0,
            w: width,
            h: height,
        },
    );
    let mut svg = "<svg class=\"treemap\" data-chart=\"treemap\" viewBox=\"0 0 1120 420\" role=\"img\" aria-label=\"File debt treemap\">".to_string();
    for (item, rect) in rects {
        if rect.w <= 1.0 || rect.h <= 1.0 {
            continue;
        }
        let inset = 2.0;
        let x = rect.x + inset;
        let y = rect.y + inset;
        let w = (rect.w - inset * 2.0).max(0.5);
        let h = (rect.h - inset * 2.0).max(0.5);
        let color = heat_color(item.score / max_score);
        let link_open = item
            .path_url
            .as_ref()
            .map(|url| {
                format!(
                    "<a href=\"{}\" target=\"_blank\" rel=\"noopener\">",
                    escape(url)
                )
            })
            .unwrap_or_default();
        let link_close = if item.path_url.is_some() { "</a>" } else { "" };
        svg.push_str(&format!(
            "<g>{}<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{:.1}\" rx=\"3\" fill=\"{}\" opacity=\".88\"><title>{}: score {:.2}; top finding: {}</title></rect>",
            link_open,
            x,
            y,
            w,
            h,
            color,
            escape(&item.path),
            item.score,
            escape(&item.top_finding)
        ));
        if w > 120.0 && h > 42.0 {
            let label = truncate_middle(&item.path, 34);
            svg.push_str(&format!(
                "<text x=\"{:.1}\" y=\"{:.1}\" fill=\"#16181d\" font-size=\"12\" font-weight=\"700\">{}</text><text x=\"{:.1}\" y=\"{:.1}\" fill=\"#16181d\" font-size=\"11\">{:.2}</text>",
                x + 8.0,
                y + 18.0,
                escape(&label),
                x + 8.0,
                y + 34.0,
                item.score
            ));
        }
        svg.push_str(link_close);
        svg.push_str("</g>");
    }
    svg.push_str("</svg>");
    svg
}

struct TopFileFinding {
    line: usize,
    summary: String,
}

fn top_file_finding(report: &SlopReport, path: &PathBuf) -> Option<TopFileFinding> {
    report
        .findings
        .iter()
        .filter(|finding| &finding.path == path)
        .max_by(|a, b| {
            finding_priority(report, a)
                .partial_cmp(&finding_priority(report, b))
                .unwrap_or(Ordering::Equal)
        })
        .map(|finding| TopFileFinding {
            line: finding.line,
            summary: format!("{} {}", finding.rule_id, finding.evidence),
        })
}

fn squarify(items: Vec<TreemapItem>, mut rect: Rect) -> Vec<(TreemapItem, Rect)> {
    let mut laid_out = Vec::new();
    let mut row: Vec<TreemapItem> = Vec::new();
    let mut index = 0;
    while index < items.len() {
        let item = items[index].clone();
        let mut candidate = row.clone();
        candidate.push(item.clone());
        let side = rect.w.min(rect.h).max(1.0);
        if row.is_empty() || worst_ratio(&candidate, side) <= worst_ratio(&row, side) {
            row.push(item);
            index += 1;
        } else {
            layout_row(&row, &mut rect, &mut laid_out);
            row.clear();
        }
    }
    if !row.is_empty() {
        layout_row(&row, &mut rect, &mut laid_out);
    }
    laid_out
}

fn worst_ratio(row: &[TreemapItem], side: f64) -> f64 {
    if row.is_empty() {
        return f64::INFINITY;
    }
    let sum = row.iter().map(|item| item.area).sum::<f64>().max(1.0);
    let min = row
        .iter()
        .map(|item| item.area)
        .fold(f64::INFINITY, f64::min)
        .max(1.0);
    let max = row.iter().map(|item| item.area).fold(0.0f64, f64::max);
    ((side * side * max) / (sum * sum)).max((sum * sum) / (side * side * min))
}

fn layout_row(row: &[TreemapItem], rect: &mut Rect, out: &mut Vec<(TreemapItem, Rect)>) {
    let area = row.iter().map(|item| item.area).sum::<f64>();
    if rect.w >= rect.h {
        let row_h = (area / rect.w).min(rect.h);
        let mut x = rect.x;
        for item in row {
            let w = if row_h > 0.0 { item.area / row_h } else { 0.0 };
            out.push((
                item.clone(),
                Rect {
                    x,
                    y: rect.y,
                    w,
                    h: row_h,
                },
            ));
            x += w;
        }
        rect.y += row_h;
        rect.h = (rect.h - row_h).max(0.0);
    } else {
        let row_w = (area / rect.h).min(rect.w);
        let mut y = rect.y;
        for item in row {
            let h = if row_w > 0.0 { item.area / row_w } else { 0.0 };
            out.push((
                item.clone(),
                Rect {
                    x: rect.x,
                    y,
                    w: row_w,
                    h,
                },
            ));
            y += h;
        }
        rect.x += row_w;
        rect.w = (rect.w - row_w).max(0.0);
    }
}

fn truncate_middle(value: &str, max_chars: usize) -> String {
    let count = value.chars().count();
    if count <= max_chars {
        return value.into();
    }
    let keep = max_chars.saturating_sub(3) / 2;
    let start = value.chars().take(keep).collect::<String>();
    let end = value
        .chars()
        .rev()
        .take(keep)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("{start}...{end}")
}

fn render_timeline_svg(report: &SlopReport) -> String {
    let points = &report.debt_timeline;
    let width = 1120.0;
    let height = 320.0;
    let left = 58.0;
    let right = 24.0;
    let top = 24.0;
    let bottom = 46.0;
    if points.is_empty() {
        return "<svg class=\"timeline\" data-chart=\"timeline\" viewBox=\"0 0 1120 320\" role=\"img\" aria-label=\"Debt timeline\"><line class=\"axis\" x1=\"58\" y1=\"274\" x2=\"1096\" y2=\"274\"/><text x=\"58\" y=\"154\" fill=\"#8d96a6\">No debt timeline data</text></svg>".into();
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
            left + ((time - min_time) as f64 / (max_time - min_time) as f64)
                * (width - left - right)
        }
    };
    let y = |debt: f64| height - bottom - (debt / max_debt) * (height - top - bottom);
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
    let mut svg = "<svg class=\"timeline\" data-chart=\"timeline\" viewBox=\"0 0 1120 320\" role=\"img\" aria-label=\"Debt cumulative timeline\"><defs><linearGradient id=\"debt-line\" x1=\"0%\" x2=\"100%\" y1=\"0%\" y2=\"0%\"><stop offset=\"0%\" stop-color=\"#7FB069\"/><stop offset=\"55%\" stop-color=\"#D8A657\"/><stop offset=\"100%\" stop-color=\"#D66A61\"/></linearGradient></defs>".to_string();
    for tick in 0..=4 {
        let value = max_debt * f64::from(tick) / 4.0;
        let yy = y(value);
        svg.push_str(&format!(
            "<line class=\"grid\" x1=\"{left}\" y1=\"{yy:.1}\" x2=\"{}\" y2=\"{yy:.1}\"/><text x=\"48\" y=\"{:.1}\" fill=\"#8d96a6\" font-size=\"11\" text-anchor=\"end\">{:.0}</text>",
            width - right,
            yy + 4.0,
            value
        ));
    }
    svg.push_str(&format!(
        "<line class=\"axis\" x1=\"{left}\" y1=\"{}\" x2=\"{}\" y2=\"{}\"/><line class=\"axis\" x1=\"{left}\" y1=\"{top}\" x2=\"{left}\" y2=\"{}\"/><polyline points=\"{}\" fill=\"none\" stroke=\"url(#debt-line)\" stroke-width=\"3\"/>",
        height - bottom,
        width - right,
        height - bottom,
        height - bottom,
        polyline
    ));
    for time in time_ticks(min_time, max_time, 5) {
        let xx = x(time);
        svg.push_str(&format!(
            "<line class=\"grid\" x1=\"{xx:.1}\" y1=\"{}\" x2=\"{xx:.1}\" y2=\"{}\"/><text x=\"{xx:.1}\" y=\"304\" fill=\"#8d96a6\" font-size=\"11\" text-anchor=\"middle\">{}</text>",
            top,
            height - bottom,
            escape(&format_month_year(time))
        ));
    }
    for point in points {
        let color = if point.ai_probability >= 0.3 {
            "#D66A61"
        } else {
            "#4FA3C7"
        };
        let commit_url = github_commit_url(report, &point.commit_oid);
        let link_open = commit_url
            .as_ref()
            .map(|url| {
                format!(
                    "<a href=\"{}\" target=\"_blank\" rel=\"noopener\">",
                    escape(url)
                )
            })
            .unwrap_or_default();
        let link_close = if commit_url.is_some() { "</a>" } else { "" };
        let data_href = commit_url
            .as_ref()
            .map(|url| format!(" data-href=\"{}\"", escape(url)))
            .unwrap_or_default();
        let title = format!(
            "{} {}: score {:.2}",
            short_oid(&point.commit_oid),
            truncate_end(commit_subject(&point.commit_message), 60),
            point.cumulative_debt_score
        );
        svg.push_str(&format!(
            "{}<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"4.5\" fill=\"{}\"{}><title>{}</title></circle>{}",
            link_open,
            x(point.commit_time),
            y(point.cumulative_debt_score),
            color,
            data_href,
            escape(&title),
            link_close
        ));
    }
    svg.push_str("<text x=\"58\" y=\"18\" fill=\"#8d96a6\" font-size=\"12\">Debt score</text><text x=\"1096\" y=\"18\" fill=\"#8d96a6\" font-size=\"12\" text-anchor=\"end\">AI-suspect commits are red</text></svg>");
    svg
}

fn time_ticks(min_time: i64, max_time: i64, count: usize) -> Vec<i64> {
    if count <= 1 || min_time == max_time {
        return vec![min_time];
    }
    (0..count)
        .map(|idx| min_time + ((max_time - min_time) * idx as i64 / (count - 1) as i64))
        .collect()
}

fn format_month_year(timestamp: i64) -> String {
    DateTime::<Utc>::from_timestamp(timestamp, 0)
        .map(|date| format!("{:02}/{}", date.month(), date.year()))
        .unwrap_or_else(|| "unknown".into())
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
    use crate::ir::{Attribution, Author, Confidence, Finding, SlopReport, Summary};
    use std::collections::BTreeMap;

    fn sample_report() -> SlopReport {
        SlopReport {
            repo_url: Some("https://github.com/example/project".into()),
            commit_messages: BTreeMap::from([("abc".into(), "add unused helper".into())]),
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
                total_loc: 100,
                total_debt_score: 3.0,
                debt_index: 30.0,
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
    fn terminal_headline_uses_debt_index() {
        let terminal = render_terminal(&sample_report());

        assert!(terminal.contains("debt index: 30.00/100"));
        assert!(!terminal.contains("\ndebt score:"));
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

    #[test]
    fn html_report_links_commits_and_files() {
        let html = render_html(&sample_report());

        assert!(html.contains("https://github.com/example/project/commit/abc"));
        assert!(html.contains("https://github.com/example/project/blob/HEAD/src/lib.rs#L2"));
        assert!(html.contains("add unused helper"));
    }

    #[test]
    fn summary_report_stays_compact() {
        let summary = render_summary(&sample_report(), Some(2.2));

        assert!(summary.lines().count() <= 5);
        assert!(summary.contains("debt index: 30.00/100 (LOW)"));
        assert!(summary.contains("scan complete in 2.2s"));
    }

    #[test]
    fn html_report_has_findings_controls_script_and_footer() {
        let footer = ReportFooter {
            version: "0.1.0".into(),
            date: "2026-07-03".into(),
            head: "abc1234".into(),
        };
        let html = render_html_with_footer(&sample_report(), Some(&footer));

        assert!(html.contains("<input id=\"findings-search\""));
        assert!(html.contains("<script>"));
        assert!(html.contains("data-rule-filter=\"SL-003\""));
        assert!(html.contains("Generated by SlopLens v0.1.0 · 2026-07-03 · HEAD=abc1234"));
    }

    #[test]
    fn ai_evidence_is_human_readable_without_scores() {
        let attr = Attribution {
            author: Author {
                name: "Bot".into(),
                email: "bot@example.com".into(),
            },
            commit_oid: "abc".into(),
            author_time: 0,
            commit_time: 0,
            ai_probability: 0.9,
            evidence: vec![
                "trailer:Co-authored-by:Copilot(+0.9)".into(),
                "shape:even churn(+0.2)".into(),
            ],
        };
        let evidence = evidence_summary(&attr);

        assert!(evidence.contains("Copilot co-author trailer detected (high confidence)"));
        assert!(evidence.contains("Even additive churn across files (low confidence)"));
        assert!(!evidence.contains("+0.9"));
    }
}
