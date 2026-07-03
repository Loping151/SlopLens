use crate::config::SlopConfig;
use crate::git_ingest::{GitHistory, is_supported_path};
use crate::ir::FileChangeKind;
use crate::ir::{Confidence, Finding, Symbol, SymbolKind, stable_fingerprint};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser};

const ANALYZE_PROGRESS_INTERVAL: usize = 100;

#[derive(Debug, Clone, Default)]
pub struct Analysis {
    pub symbols: Vec<Symbol>,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone)]
struct FunctionBlock {
    name: String,
    path: PathBuf,
    line: usize,
    end_line: usize,
    text: String,
}

pub fn analyze_repo(repo_path: &Path, history: &GitHistory) -> Result<Analysis> {
    analyze_repo_with_config(repo_path, history, &SlopConfig::default())
}

pub fn analyze_repo_for_paths(
    repo_path: &Path,
    history: &GitHistory,
    changed_paths: &HashSet<PathBuf>,
) -> Result<Analysis> {
    analyze_repo_for_paths_with_config(repo_path, history, changed_paths, &SlopConfig::default())
}

pub fn analyze_repo_with_config(
    repo_path: &Path,
    history: &GitHistory,
    config: &SlopConfig,
) -> Result<Analysis> {
    analyze_repo_with_config_and_progress(repo_path, history, config, true)
}

pub fn analyze_repo_with_config_and_progress(
    repo_path: &Path,
    history: &GitHistory,
    config: &SlopConfig,
    emit_progress: bool,
) -> Result<Analysis> {
    analyze_repo_filtered(repo_path, history, None, config, emit_progress)
}

pub fn analyze_repo_for_paths_with_config(
    repo_path: &Path,
    history: &GitHistory,
    changed_paths: &HashSet<PathBuf>,
    config: &SlopConfig,
) -> Result<Analysis> {
    analyze_repo_for_paths_with_config_and_progress(repo_path, history, changed_paths, config, true)
}

pub fn analyze_repo_for_paths_with_config_and_progress(
    repo_path: &Path,
    history: &GitHistory,
    changed_paths: &HashSet<PathBuf>,
    config: &SlopConfig,
    emit_progress: bool,
) -> Result<Analysis> {
    analyze_repo_filtered(
        repo_path,
        history,
        Some(changed_paths),
        config,
        emit_progress,
    )
}

fn analyze_repo_filtered(
    repo_path: &Path,
    history: &GitHistory,
    path_filter: Option<&HashSet<PathBuf>>,
    config: &SlopConfig,
    emit_progress: bool,
) -> Result<Analysis> {
    let files = collect_supported_files(repo_path, path_filter, config)?;
    let introduced = introduced_by_map(history);
    let mut all_symbols = Vec::new();
    let mut functions = Vec::new();
    let mut file_texts = HashMap::new();

    let total_files = files.len();
    for (idx, file) in files.into_iter().enumerate() {
        let processed = idx + 1;
        if emit_progress && (processed == total_files || processed % ANALYZE_PROGRESS_INTERVAL == 0)
        {
            eprintln!("analyzing files: {processed}/{total_files}");
        }
        let rel = file.strip_prefix(repo_path).unwrap_or(&file).to_path_buf();
        let text = fs::read_to_string(&file)
            .with_context(|| format!("failed to read {}", file.display()))?;
        let parsed = parse_file(&rel, &text)?;
        all_symbols.extend(parsed.0);
        functions.extend(parsed.1);
        file_texts.insert(rel, text);
    }

    let mut findings = Vec::new();
    if config.rule_enabled("SL-001") {
        findings.extend(dead_code_candidates(&all_symbols, &file_texts, &introduced));
    }
    if config.rule_enabled("SL-002") {
        findings.extend(duplication_candidates(&functions, &introduced));
    }
    if config.rule_enabled("SL-003") {
        findings.extend(complexity_candidates(
            &functions,
            &introduced,
            config.sl003_threshold(),
        ));
    }
    if config.rule_enabled("SL-004") {
        findings.extend(comment_inflation_candidates(
            &file_texts,
            history,
            &introduced,
        ));
    }

    Ok(Analysis {
        symbols: all_symbols,
        findings,
    })
}

fn collect_supported_files(
    repo_path: &Path,
    path_filter: Option<&HashSet<PathBuf>>,
    config: &SlopConfig,
) -> Result<Vec<PathBuf>> {
    fn visit(
        repo_path: &Path,
        dir: &Path,
        path_filter: Option<&HashSet<PathBuf>>,
        config: &SlopConfig,
        out: &mut Vec<PathBuf>,
    ) -> Result<()> {
        for entry in
            fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                let rel = path.strip_prefix(repo_path).unwrap_or(&path);
                if name == ".git" || config.is_ignored(rel) {
                    continue;
                }
                visit(repo_path, &path, path_filter, config, out)?;
            } else if is_supported_path(&path.to_string_lossy()) {
                let rel = path.strip_prefix(repo_path).unwrap_or(&path);
                if !config.is_ignored(rel) && path_filter.is_none_or(|filter| filter.contains(rel))
                {
                    out.push(path);
                }
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    visit(repo_path, repo_path, path_filter, config, &mut files)?;
    Ok(files)
}

fn parse_file(path: &Path, text: &str) -> Result<(Vec<Symbol>, Vec<FunctionBlock>)> {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("rs") => parse_tree_sitter(path, text, LanguageKind::Rust),
        Some("go") => parse_tree_sitter(path, text, LanguageKind::Go),
        Some("py") => parse_tree_sitter(path, text, LanguageKind::Python),
        Some("js") => parse_tree_sitter(path, text, LanguageKind::JavaScript),
        _ => Ok(parse_fallback(path, text)),
    }
}

#[derive(Debug, Clone, Copy)]
enum LanguageKind {
    Rust,
    Go,
    Python,
    JavaScript,
}

fn parse_tree_sitter(
    path: &Path,
    text: &str,
    language_kind: LanguageKind,
) -> Result<(Vec<Symbol>, Vec<FunctionBlock>)> {
    let mut parser = Parser::new();
    match language_kind {
        LanguageKind::Rust => {
            let language = tree_sitter_rust::LANGUAGE.into();
            parser.set_language(&language)?;
        }
        LanguageKind::Go => {
            let language = tree_sitter_go::LANGUAGE.into();
            parser.set_language(&language)?;
        }
        LanguageKind::Python => {
            let language = tree_sitter_python::LANGUAGE.into();
            parser.set_language(&language)?;
        }
        LanguageKind::JavaScript => {
            let language = tree_sitter_javascript::LANGUAGE.into();
            parser.set_language(&language)?;
        }
    }
    let tree = parser
        .parse(text, None)
        .context("tree-sitter parse failed")?;
    let mut symbols = Vec::new();
    let mut functions = Vec::new();
    collect_nodes(
        tree.root_node(),
        path,
        text,
        language_kind,
        &mut symbols,
        &mut functions,
    );
    Ok((symbols, functions))
}

fn collect_nodes(
    node: Node<'_>,
    path: &Path,
    text: &str,
    language_kind: LanguageKind,
    symbols: &mut Vec<Symbol>,
    functions: &mut Vec<FunctionBlock>,
) {
    let kind = node.kind();
    let is_function = matches!(
        (language_kind, kind),
        (LanguageKind::Rust, "function_item")
            | (LanguageKind::Go, "function_declaration")
            | (LanguageKind::Go, "method_declaration")
            | (LanguageKind::Python, "function_definition")
            | (LanguageKind::JavaScript, "function_declaration")
            | (LanguageKind::JavaScript, "generator_function_declaration")
            | (LanguageKind::JavaScript, "method_definition")
    );
    let is_type = matches!(
        (language_kind, kind),
        (LanguageKind::Rust, "struct_item")
            | (LanguageKind::Rust, "enum_item")
            | (LanguageKind::Go, "type_declaration")
            | (LanguageKind::Python, "class_definition")
            | (LanguageKind::JavaScript, "class_declaration")
    );
    let is_variable = matches!(
        (language_kind, kind),
        (LanguageKind::Rust, "const_item")
            | (LanguageKind::Rust, "static_item")
            | (LanguageKind::Go, "var_declaration")
            | (LanguageKind::Go, "const_declaration")
            | (LanguageKind::JavaScript, "variable_declaration")
            | (LanguageKind::JavaScript, "lexical_declaration")
    );

    if (is_function || is_type || is_variable)
        && let Some(name) = node_name(node, text, language_kind)
    {
        let line = node.start_position().row + 1;
        let end_line = node.end_position().row + 1;
        let raw = node
            .utf8_text(text.as_bytes())
            .unwrap_or_default()
            .to_string();
        let exported = is_exported_name(&name, &raw, language_kind);
        let symbol_kind = if is_function {
            if is_method_node(node, language_kind) {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            }
        } else if is_type {
            SymbolKind::Class
        } else {
            SymbolKind::Variable
        };
        symbols.push(Symbol {
            name: name.clone(),
            kind: symbol_kind,
            file: path.to_path_buf(),
            line,
            is_exported: exported,
        });
        if is_function {
            let raw = if matches!(language_kind, LanguageKind::Python) {
                python_block_text(node, text)
            } else {
                raw
            };
            functions.push(FunctionBlock {
                name,
                path: path.to_path_buf(),
                line,
                end_line,
                text: raw,
            });
        }
    }

    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i) {
            collect_nodes(child, path, text, language_kind, symbols, functions);
        }
    }
}

fn is_method_node(node: Node<'_>, language_kind: LanguageKind) -> bool {
    if matches!(language_kind, LanguageKind::Go | LanguageKind::JavaScript)
        && matches!(node.kind(), "method_declaration" | "method_definition")
    {
        return true;
    }
    if !matches!(language_kind, LanguageKind::Python) || node.kind() != "function_definition" {
        return false;
    }
    let mut parent = node.parent();
    while let Some(ancestor) = parent {
        match ancestor.kind() {
            "class_definition" => return true,
            "function_definition" => return false,
            _ => parent = ancestor.parent(),
        }
    }
    false
}

fn node_name(node: Node<'_>, text: &str, language_kind: LanguageKind) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        let value = name.utf8_text(text.as_bytes()).ok()?.trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    if matches!(language_kind, LanguageKind::Go) && node.kind() == "method_declaration" {
        for i in 0..node.named_child_count() {
            let child = node.named_child(i)?;
            if child.kind() == "field_identifier" || child.kind() == "identifier" {
                let value = child.utf8_text(text.as_bytes()).ok()?.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    if matches!(language_kind, LanguageKind::JavaScript)
        && matches!(node.kind(), "variable_declaration" | "lexical_declaration")
    {
        for i in 0..node.named_child_count() {
            let child = node.named_child(i)?;
            if child.kind() != "variable_declarator" {
                continue;
            }
            if let Some(name) = child.child_by_field_name("name") {
                let value = name.utf8_text(text.as_bytes()).ok()?.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    for i in 0..node.named_child_count() {
        let child = node.named_child(i)?;
        if matches!(
            child.kind(),
            "identifier" | "type_identifier" | "field_identifier"
        ) {
            let value = child.utf8_text(text.as_bytes()).ok()?.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn python_block_text(node: Node<'_>, text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = node.start_position().row;
    if start >= lines.len() {
        return text.to_string();
    }
    let mut end = node.end_position().row + 1;
    for (idx, line) in lines.iter().enumerate().skip(end) {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.starts_with('@') && visual_indent(line) <= visual_indent(lines[start]) {
            end = idx;
            break;
        }
    }
    lines[start..end.min(lines.len())].join("\n")
}

fn parse_fallback(path: &Path, text: &str) -> (Vec<Symbol>, Vec<FunctionBlock>) {
    let mut symbols = Vec::new();
    let mut functions = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let is_python = path.extension().and_then(|ext| ext.to_str()) == Some("py");
    let mut class_indents = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let indent = visual_indent(line);
        while class_indents.last().is_some_and(|class_indent| {
            !trimmed.is_empty() && indent <= *class_indent && !trimmed.starts_with('@')
        }) {
            class_indents.pop();
        }
        let (kind, rest) = if let Some(rest) = trimmed.strip_prefix("async def ") {
            let kind = if is_python
                && class_indents
                    .last()
                    .is_some_and(|class_indent| indent > *class_indent)
            {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            (Some(kind), rest)
        } else if let Some(rest) = trimmed.strip_prefix("def ") {
            let kind = if is_python
                && class_indents
                    .last()
                    .is_some_and(|class_indent| indent > *class_indent)
            {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            (Some(kind), rest)
        } else if let Some(rest) = trimmed.strip_prefix("async function ") {
            (Some(SymbolKind::Function), rest)
        } else if let Some(rest) = trimmed.strip_prefix("function ") {
            (Some(SymbolKind::Function), rest)
        } else if let Some(rest) = trimmed.strip_prefix("class ") {
            (Some(SymbolKind::Class), rest)
        } else {
            (None, "")
        };
        let Some(kind) = kind else {
            continue;
        };
        let name = rest
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .next()
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let line_no = idx + 1;
        let exported = !name.starts_with('_');
        if kind == SymbolKind::Class {
            class_indents.push(indent);
        }
        symbols.push(Symbol {
            name: name.to_string(),
            kind: kind.clone(),
            file: path.to_path_buf(),
            line: line_no,
            is_exported: exported,
        });
        if matches!(kind, SymbolKind::Function | SymbolKind::Method) {
            let end = estimate_fallback_function_end(&lines, idx);
            functions.push(FunctionBlock {
                name: name.to_string(),
                path: path.to_path_buf(),
                line: line_no,
                end_line: end,
                text: lines[idx..end.min(lines.len())].join("\n"),
            });
        }
    }
    (symbols, functions)
}

fn estimate_fallback_function_end(lines: &[&str], start: usize) -> usize {
    let start_indent = visual_indent(lines[start]);
    for (idx, line) in lines.iter().enumerate().skip(start + 1) {
        if !line.trim().is_empty() && visual_indent(line) <= start_indent {
            return idx;
        }
    }
    lines.len()
}

fn dead_code_candidates(
    symbols: &[Symbol],
    file_texts: &HashMap<PathBuf, String>,
    introduced: &HashMap<PathBuf, String>,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for symbol in symbols {
        if !matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method) {
            continue;
        }
        if symbol.is_exported {
            continue;
        }
        if should_skip_dead_code_symbol(symbol, file_texts) {
            continue;
        }
        let count = file_texts
            .values()
            .map(|text| identifier_occurrences(text, &symbol.name))
            .sum::<usize>();
        if count > 1 {
            continue;
        }
        let confidence = if symbol.file.extension().and_then(|ext| ext.to_str()) == Some("go") {
            Confidence::Medium
        } else {
            Confidence::High
        };
        findings.push(finding(FindingSpec {
            rule_id: "SL-001",
            path: &symbol.file,
            line: symbol.line,
            symbol_name: Some(&symbol.name),
            severity: 3,
            confidence,
            evidence: format!(
                "unused candidate: private `{}` has no references across analyzed files",
                symbol.name
            ),
            introduced_by: introduced.get(&symbol.file).cloned(),
        }));
    }
    findings
}

fn should_skip_dead_code_symbol(symbol: &Symbol, file_texts: &HashMap<PathBuf, String>) -> bool {
    let is_python = symbol.file.extension().and_then(|ext| ext.to_str()) == Some("py");
    if !is_python {
        return false;
    }
    if is_python_dunder(&symbol.name) {
        return true;
    }
    file_texts
        .get(&symbol.file)
        .is_some_and(|text| has_decorator_before(text, symbol.line))
}

fn is_python_dunder(name: &str) -> bool {
    matches!(
        name,
        "__init__"
            | "__new__"
            | "__call__"
            | "__hash__"
            | "__eq__"
            | "__ne__"
            | "__lt__"
            | "__le__"
            | "__gt__"
            | "__ge__"
            | "__len__"
            | "__iter__"
            | "__next__"
            | "__enter__"
            | "__exit__"
            | "__aenter__"
            | "__aexit__"
            | "__str__"
            | "__repr__"
            | "__bool__"
            | "__bytes__"
            | "__getattr__"
            | "__getattribute__"
            | "__setattr__"
            | "__delattr__"
            | "__getitem__"
            | "__setitem__"
            | "__delitem__"
            | "__contains__"
    )
}

fn has_decorator_before(text: &str, line: usize) -> bool {
    if line <= 1 {
        return false;
    }
    text.lines()
        .nth(line - 2)
        .is_some_and(|previous| previous.trim_start().starts_with('@'))
}

fn duplication_candidates(
    functions: &[FunctionBlock],
    introduced: &HashMap<PathBuf, String>,
) -> Vec<Finding> {
    let mut seen: HashMap<String, &FunctionBlock> = HashMap::new();
    let mut findings = Vec::new();
    for block in functions {
        let comparable = comparable_function_text(block);
        let tokens = normalized_tokens(comparable);
        if tokens.len() < 24 {
            continue;
        }
        let key = tokens.join(" ");
        let key = format!("{key}\x1f{}", string_literal_signature(comparable));
        if let Some(first) = seen.get(&key) {
            findings.push(finding(FindingSpec {
                rule_id: "SL-002",
                path: &block.path,
                line: block.line,
                symbol_name: Some(&block.name),
                severity: 4,
                confidence: Confidence::High,
                evidence: format!(
                    "duplicate block: `{}` matches `{}` in {}:{}",
                    block.name,
                    first.name,
                    first.path.display(),
                    first.line
                ),
                introduced_by: introduced.get(&block.path).cloned(),
            }));
        } else {
            seen.insert(key, block);
        }
    }
    findings
}

fn comparable_function_text(block: &FunctionBlock) -> &str {
    if block.path.extension().and_then(|ext| ext.to_str()) == Some("py") {
        return block
            .text
            .split_once('\n')
            .map(|(_, body)| body)
            .unwrap_or("");
    }
    block
        .text
        .split_once('{')
        .map(|(_, body)| body)
        .unwrap_or(&block.text)
}

fn complexity_candidates(
    functions: &[FunctionBlock],
    introduced: &HashMap<PathBuf, String>,
    line_threshold: usize,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    for block in functions {
        let line_count = block.end_line.saturating_sub(block.line) + 1;
        let nesting = max_nesting(&block.text, &block.path);
        let cyclomatic = approximate_cyclomatic(&block.text);
        if line_count >= line_threshold || nesting >= 6 || cyclomatic >= 12 {
            let severity = if line_count >= line_threshold.saturating_add(60) || cyclomatic >= 18 {
                5
            } else {
                4
            };
            findings.push(finding(FindingSpec {
                rule_id: "SL-003",
                path: &block.path,
                line: block.line,
                symbol_name: Some(&block.name),
                severity,
                confidence: Confidence::Medium,
                evidence: format!(
                    "complexity candidate: `{}` spans {line_count} lines, nesting {nesting}, cyclomatic approx {cyclomatic}",
                    block.name
                ),
                introduced_by: introduced.get(&block.path).cloned(),
            }));
        }
    }
    findings
}

fn comment_inflation_candidates(
    file_texts: &HashMap<PathBuf, String>,
    history: &GitHistory,
    introduced: &HashMap<PathBuf, String>,
) -> Vec<Finding> {
    let mut historical: HashMap<PathBuf, Vec<f64>> = HashMap::new();
    for hunk in &history.hunks {
        let mut comments = 0usize;
        let mut code = 0usize;
        for line in &hunk.added {
            classify_line(line, &mut comments, &mut code);
        }
        if code + comments >= 5 {
            historical
                .entry(hunk.path.clone())
                .or_default()
                .push(ratio(comments, code));
        }
    }

    let mut findings = Vec::new();
    for (path, text) in file_texts {
        let mut comments = 0usize;
        let mut code = 0usize;
        for line in text.lines() {
            classify_line(line, &mut comments, &mut code);
        }
        if code < 20 {
            continue;
        }
        let current = ratio(comments, code);
        let baseline = historical
            .get(path)
            .map(|values| values.iter().sum::<f64>() / values.len().max(1) as f64)
            .unwrap_or(0.15);
        if current >= 0.45 && current > (baseline * 2.0).max(0.25) {
            findings.push(finding(FindingSpec {
                rule_id: "SL-004",
                path,
                line: 1,
                symbol_name: None,
                severity: 2,
                confidence: Confidence::Medium,
                evidence: format!(
                    "comment inflation candidate: comment/code ratio {current:.2}, historical added-line baseline {baseline:.2}"
                ),
                introduced_by: introduced.get(path).cloned(),
            }));
        }
    }
    findings
}

fn classify_line(line: &str, comments: &mut usize, code: &mut usize) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    if trimmed.starts_with("//")
        || trimmed.starts_with('#')
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with("*/")
    {
        *comments += 1;
    } else {
        *code += 1;
    }
}

fn ratio(comments: usize, code: usize) -> f64 {
    comments as f64 / code.max(1) as f64
}

fn identifier_occurrences(text: &str, needle: &str) -> usize {
    let mut count = 0;
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            current.push(ch);
        } else {
            if current == needle {
                count += 1;
            }
            current.clear();
        }
    }
    if current == needle {
        count += 1;
    }
    count
}

fn normalized_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if matches!(ch, '"' | '\'' | '`') {
            flush_token(&mut current, &mut tokens);
            consume_string_literal(ch, &mut chars);
            tokens.push("STR".into());
            continue;
        }
        if ch.is_ascii_alphanumeric() || ch == '_' {
            current.push(ch.to_ascii_lowercase());
        } else {
            flush_token(&mut current, &mut tokens);
            if (ch == '&' || ch == '|') && chars.peek() == Some(&ch) {
                chars.next();
                tokens.push(format!("{ch}{ch}"));
            } else if matches!(
                ch,
                '{' | '}' | '(' | ')' | '+' | '-' | '*' | '/' | '=' | '<' | '>' | '?'
            ) {
                tokens.push(ch.to_string());
            }
        }
    }
    flush_token(&mut current, &mut tokens);
    tokens
}

fn flush_token(current: &mut String, tokens: &mut Vec<String>) {
    if current.is_empty() {
        return;
    }
    if current.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        tokens.push("N".into());
    } else if !matches!(current.as_str(), "fn" | "func" | "pub") {
        tokens.push(current.clone());
    }
    current.clear();
}

fn consume_string_literal<I>(quote: char, chars: &mut std::iter::Peekable<I>)
where
    I: Iterator<Item = char>,
{
    let mut escaped = false;
    for ch in chars.by_ref() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            break;
        }
    }
}

fn string_literal_signature(text: &str) -> String {
    let mut literals = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if matches!(ch, '"' | '\'' | '`') {
            let literal = collect_string_literal(ch, &mut chars);
            literals.push(literal);
        }
    }
    literals.join("\x1e")
}

fn collect_string_literal<I>(quote: char, chars: &mut std::iter::Peekable<I>) -> String
where
    I: Iterator<Item = char>,
{
    let mut literal = String::new();
    let mut escaped = false;
    for ch in chars.by_ref() {
        if escaped {
            literal.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            break;
        }
        literal.push(ch);
    }
    literal
}

fn max_nesting(text: &str, path: &Path) -> usize {
    if path.extension().and_then(|ext| ext.to_str()) == Some("py") {
        return max_python_nesting(text);
    }
    let mut depth = 0usize;
    let mut max = 0usize;
    for ch in text.chars() {
        match ch {
            '{' => {
                depth += 1;
                max = max.max(depth);
            }
            '}' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max.saturating_sub(1)
}

fn max_python_nesting(text: &str) -> usize {
    let mut meaningful_lines = text
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'));
    let base_indent = meaningful_lines.next().map(visual_indent).unwrap_or(0);
    meaningful_lines
        .map(|line| visual_indent(line).saturating_sub(base_indent).div_ceil(4))
        .max()
        .unwrap_or(0)
}

fn visual_indent(line: &str) -> usize {
    line.chars()
        .take_while(|c| c.is_whitespace())
        .map(|c| if c == '\t' { 4 } else { 1 })
        .sum()
}

fn approximate_cyclomatic(text: &str) -> usize {
    let tokens = normalized_tokens(text);
    1 + tokens
        .iter()
        .filter(|token| {
            matches!(
                token.as_str(),
                "if" | "elif"
                    | "for"
                    | "while"
                    | "match"
                    | "case"
                    | "except"
                    | "and"
                    | "or"
                    | "&&"
                    | "||"
            )
        })
        .count()
}

fn is_exported_name(name: &str, raw: &str, language_kind: LanguageKind) -> bool {
    match language_kind {
        LanguageKind::Rust => raw.trim_start().starts_with("pub "),
        LanguageKind::Go => name.chars().next().is_some_and(char::is_uppercase),
        LanguageKind::Python | LanguageKind::JavaScript => !name.starts_with('_'),
    }
}

fn introduced_by_map(history: &GitHistory) -> HashMap<PathBuf, String> {
    let changes_by_commit = changes_by_commit(history);
    let mut map = HashMap::new();
    for change in &history.changes {
        if is_pure_rename_commit(&changes_by_commit, &change.commit_oid) {
            continue;
        }
        map.entry(change.path.clone())
            .or_insert_with(|| change.commit_oid.clone());
    }
    map
}

fn changes_by_commit(history: &GitHistory) -> HashMap<&str, Vec<&crate::ir::FileChange>> {
    let mut by_commit: HashMap<&str, Vec<&crate::ir::FileChange>> = HashMap::new();
    for change in &history.changes {
        by_commit
            .entry(change.commit_oid.as_str())
            .or_default()
            .push(change);
    }
    by_commit
}

fn is_pure_rename_commit(
    changes_by_commit: &HashMap<&str, Vec<&crate::ir::FileChange>>,
    oid: &str,
) -> bool {
    let Some(changes) = changes_by_commit.get(oid) else {
        return false;
    };
    changes.iter().all(|change| {
        change.kind == FileChangeKind::Rename
            && change.lines_added.saturating_add(change.lines_deleted) <= 1
    })
}

struct FindingSpec<'a> {
    rule_id: &'a str,
    path: &'a Path,
    line: usize,
    symbol_name: Option<&'a str>,
    severity: u8,
    confidence: Confidence,
    evidence: String,
    introduced_by: Option<String>,
}

fn finding(spec: FindingSpec<'_>) -> Finding {
    let line_s = spec.line.to_string();
    let path_s = spec.path.to_string_lossy();
    let symbol = spec.symbol_name.unwrap_or_default();
    let fingerprint = stable_fingerprint(&[spec.rule_id, &path_s, &line_s, symbol]);
    Finding {
        rule_id: spec.rule_id.to_string(),
        path: spec.path.to_path_buf(),
        line: spec.line,
        symbol_name: spec.symbol_name.map(str::to_string),
        severity: spec.severity.clamp(1, 5),
        confidence: spec.confidence,
        evidence: spec.evidence,
        introduced_by: spec.introduced_by,
        fingerprint,
        status: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuleConfig;
    use crate::git_ingest::GitHistory;
    use crate::ir::{Author, Commit, FileChange, FileChangeKind};
    use std::collections::BTreeMap;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("slop-lens-analyze-test-{stamp}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn history_for(path: &str) -> GitHistory {
        GitHistory {
            repo_url: None,
            commits: vec![Commit {
                oid: "abc".into(),
                author: Author {
                    name: "A".into(),
                    email: "a@example.com".into(),
                },
                author_time: 1,
                commit_time: 1,
                message: "init".into(),
                trailers: vec![],
            }],
            changes: vec![FileChange {
                commit_oid: "abc".into(),
                path: PathBuf::from(path),
                kind: FileChangeKind::Add,
                lines_added: 1,
                lines_deleted: 0,
            }],
            hunks: vec![],
        }
    }

    fn history_with_changes(changes: Vec<(&str, &str)>) -> GitHistory {
        GitHistory {
            repo_url: None,
            commits: changes
                .iter()
                .map(|(oid, _)| Commit {
                    oid: (*oid).into(),
                    author: Author {
                        name: "A".into(),
                        email: "a@example.com".into(),
                    },
                    author_time: 1,
                    commit_time: 1,
                    message: "change".into(),
                    trailers: vec![],
                })
                .collect(),
            changes: changes
                .into_iter()
                .map(|(oid, path)| FileChange {
                    commit_oid: oid.into(),
                    path: PathBuf::from(path),
                    kind: FileChangeKind::Modify,
                    lines_added: 1,
                    lines_deleted: 0,
                })
                .collect(),
            hunks: vec![],
        }
    }

    #[test]
    fn rust_symbols_and_dead_candidate() {
        let dir = temp_dir();
        fs::write(
            dir.join("lib.rs"),
            "fn unused_local() { let x = 1; }\nfn caller() { println!(\"hi\"); }\n",
        )
        .unwrap();
        let result = analyze_repo(&dir, &history_for("lib.rs")).unwrap();
        assert!(result.symbols.iter().any(|s| s.name == "unused_local"));
        assert!(result.findings.iter().any(|f| f.rule_id == "SL-001"));
    }

    #[test]
    fn duplicate_functions_are_flagged() {
        let dir = temp_dir();
        let body = "let mut total = 0; for value in input { if value > 1 { total += value; } else { total += 1; } } total";
        fs::write(
            dir.join("lib.rs"),
            format!(
                "fn a(input: &[i32]) -> i32 {{ {body} }}\nfn b(input: &[i32]) -> i32 {{ {body} }}"
            ),
        )
        .unwrap();
        let result = analyze_repo(&dir, &history_for("lib.rs")).unwrap();
        assert!(result.findings.iter().any(|f| f.rule_id == "SL-002"));
    }

    #[test]
    fn complexity_is_flagged() {
        let dir = temp_dir();
        let mut code = String::from("fn hard(x: i32) -> i32 {\nlet mut y = x;\n");
        for _ in 0..12 {
            code.push_str("if y > 0 { y -= 1; }\n");
        }
        code.push_str("y\n}\n");
        fs::write(dir.join("lib.rs"), code).unwrap();
        let result = analyze_repo(&dir, &history_for("lib.rs")).unwrap();
        assert!(result.findings.iter().any(|f| f.rule_id == "SL-003"));
    }

    #[test]
    fn comment_inflation_is_flagged() {
        let dir = temp_dir();
        let mut code = String::new();
        for i in 0..25 {
            code.push_str(&format!("// explanatory comment {i}\n"));
        }
        for i in 0..30 {
            code.push_str(&format!("let _x{i} = {i};\n"));
        }
        fs::write(dir.join("lib.rs"), format!("fn noisy() {{\n{code}\n}}\n")).unwrap();
        let result = analyze_repo(&dir, &history_for("lib.rs")).unwrap();
        assert!(result.findings.iter().any(|f| f.rule_id == "SL-004"));
    }

    #[test]
    fn changed_path_filter_limits_analyzed_files() {
        let dir = temp_dir();
        fs::write(dir.join("changed.rs"), "fn changed_only() {}\n").unwrap();
        fs::write(dir.join("unchanged.rs"), "fn untouched() {}\n").unwrap();
        let history = history_with_changes(vec![("abc", "changed.rs")]);
        let changed_paths = HashSet::from([PathBuf::from("changed.rs")]);

        let result = analyze_repo_for_paths(&dir, &history, &changed_paths).unwrap();

        assert!(result.symbols.iter().any(|s| s.name == "changed_only"));
        assert!(!result.symbols.iter().any(|s| s.name == "untouched"));
    }

    #[test]
    fn dead_code_uses_cross_file_references() {
        let dir = temp_dir();
        fs::write(dir.join("a.rs"), "fn helper() -> i32 { 1 }\n").unwrap();
        fs::write(dir.join("b.rs"), "fn caller() -> i32 { helper() }\n").unwrap();
        let history = history_with_changes(vec![("abc", "a.rs"), ("abc", "b.rs")]);

        let result = analyze_repo(&dir, &history).unwrap();

        assert!(
            !result
                .findings
                .iter()
                .any(|f| f.rule_id == "SL-001" && f.evidence.contains("helper"))
        );
    }

    #[test]
    fn public_function_referenced_across_files_is_not_dead_code() {
        let dir = temp_dir();
        fs::write(dir.join("a.rs"), "pub fn shared_helper() -> i32 { 1 }\n").unwrap();
        fs::write(
            dir.join("b.rs"),
            "mod a;\nfn caller() -> i32 { a::shared_helper() }\n",
        )
        .unwrap();
        let history = history_with_changes(vec![("abc", "a.rs"), ("abc", "b.rs")]);

        let result = analyze_repo(&dir, &history).unwrap();

        assert!(!result.findings.iter().any(|f| {
            f.rule_id == "SL-001" && f.symbol_name.as_deref() == Some("shared_helper")
        }));
    }

    #[test]
    fn python_indentation_counts_as_nesting() {
        let dir = temp_dir();
        fs::write(
            dir.join("mod.py"),
            "async def hard(x):\n    if x:\n        for a in x:\n            while a:\n                try:\n                    if a and x or a:\n                        pass\n                except Exception:\n                    pass\n    elif x == 0:\n        pass\n",
        )
        .unwrap();

        let result = analyze_repo(&dir, &history_for("mod.py")).unwrap();

        assert!(result.findings.iter().any(|f| f.rule_id == "SL-003"));
    }

    #[test]
    fn decorated_python_function_is_not_dead_code() {
        let dir = temp_dir();
        fs::write(
            dir.join("mod.py"),
            "def route(path):\n    def wrap(fn):\n        return fn\n    return wrap\n\n@route('/ready')\ndef _ready_check():\n    return 'ok'\n",
        )
        .unwrap();

        let result = analyze_repo(&dir, &history_for("mod.py")).unwrap();

        assert!(!result.findings.iter().any(|f| {
            f.rule_id == "SL-001" && f.symbol_name.as_deref() == Some("_ready_check")
        }));
    }

    #[test]
    fn python_functions_with_different_chinese_copy_are_not_duplicates() {
        let dir = temp_dir();
        let common_body = "    total = 0\n    for value in values:\n        if value > 1:\n            total += value\n        else:\n            total += 1\n    message = ";
        fs::write(
            dir.join("copy.py"),
            format!(
                "def render_first(values):\n{common_body}'提交成功，请继续下一步'\n    return f'{{message}}: {{total}}'\n\n\
def render_second(values):\n{common_body}'保存失败，请稍后重试'\n    return f'{{message}}: {{total}}'\n"
            ),
        )
        .unwrap();

        let result = analyze_repo(&dir, &history_for("copy.py")).unwrap();

        assert!(!result.findings.iter().any(|f| {
            f.rule_id == "SL-002" && f.symbol_name.as_deref() == Some("render_second")
        }));
    }

    #[test]
    fn introduced_by_uses_first_change_for_path() {
        let dir = temp_dir();
        fs::write(dir.join("lib.rs"), "fn unused_local() {}\n").unwrap();
        let history = history_with_changes(vec![("first", "lib.rs"), ("second", "lib.rs")]);

        let result = analyze_repo(&dir, &history).unwrap();
        let finding = result
            .findings
            .iter()
            .find(|f| f.rule_id == "SL-001")
            .unwrap();

        assert_eq!(finding.introduced_by.as_deref(), Some("first"));
    }

    #[test]
    fn pure_rename_commit_is_not_used_as_introducer() {
        let dir = temp_dir();
        fs::write(dir.join("new.py"), "def _unused_local():\n    return 1\n").unwrap();
        let history = GitHistory {
            repo_url: None,
            commits: vec![Commit {
                oid: "rename".into(),
                author: Author {
                    name: "A".into(),
                    email: "a@example.com".into(),
                },
                author_time: 1,
                commit_time: 1,
                message: "rename file".into(),
                trailers: vec![],
            }],
            changes: vec![FileChange {
                commit_oid: "rename".into(),
                path: PathBuf::from("new.py"),
                kind: FileChangeKind::Rename,
                lines_added: 0,
                lines_deleted: 0,
            }],
            hunks: vec![],
        };

        let result = analyze_repo(&dir, &history).unwrap();
        let finding = result
            .findings
            .iter()
            .find(|f| f.rule_id == "SL-001")
            .unwrap();

        assert_eq!(finding.introduced_by, None);
    }

    #[test]
    fn normalized_tokens_preserve_string_literal_boundaries() {
        let tokens = normalized_tokens("print('强化⚡')\nreturn \"中文\"");

        assert_eq!(tokens, vec!["print", "(", "STR", ")", "return", "STR"]);
    }

    #[test]
    fn python_dunder_and_decorated_callbacks_are_not_dead_code() {
        let dir = temp_dir();
        fs::write(
            dir.join("mod.py"),
            "class Item:\n    def __hash__(self):\n        return 1\n\n@router.get('/x')\ndef _callback():\n    return 2\n",
        )
        .unwrap();

        let result = analyze_repo(&dir, &history_for("mod.py")).unwrap();

        assert!(
            !result
                .findings
                .iter()
                .any(|f| f.rule_id == "SL-001" && f.symbol_name.as_deref() == Some("__hash__"))
        );
        assert!(
            !result
                .findings
                .iter()
                .any(|f| f.rule_id == "SL-001" && f.symbol_name.as_deref() == Some("_callback"))
        );
    }

    #[test]
    fn config_ignore_paths_excludes_files_from_analysis() {
        let dir = temp_dir();
        fs::create_dir_all(dir.join("generated")).unwrap();
        fs::write(dir.join("generated/lib.rs"), "fn ignored_unused() {}\n").unwrap();
        fs::write(dir.join("kept.rs"), "fn kept_unused() {}\n").unwrap();
        let config = SlopConfig {
            ignore_paths: vec!["generated".into()],
            rules: BTreeMap::new(),
        };
        let history = history_with_changes(vec![("abc", "generated/lib.rs"), ("abc", "kept.rs")]);

        let result = analyze_repo_with_config(&dir, &history, &config).unwrap();

        assert!(!result.symbols.iter().any(|s| s.name == "ignored_unused"));
        assert!(result.symbols.iter().any(|s| s.name == "kept_unused"));
    }

    #[test]
    fn config_can_disable_rule_and_lower_complexity_threshold() {
        let dir = temp_dir();
        let code =
            "fn medium() {\nlet x = 1;\nlet y = 2;\nlet z = x + y;\nprintln!(\"{}\", z);\n}\n";
        fs::write(dir.join("lib.rs"), code).unwrap();
        let config = SlopConfig {
            ignore_paths: vec![],
            rules: BTreeMap::from([
                (
                    "SL-001".into(),
                    RuleConfig {
                        enabled: Some(false),
                        threshold: None,
                    },
                ),
                (
                    "SL-003".into(),
                    RuleConfig {
                        enabled: None,
                        threshold: Some(4),
                    },
                ),
            ]),
        };

        let result = analyze_repo_with_config(&dir, &history_for("lib.rs"), &config).unwrap();

        assert!(!result.findings.iter().any(|f| f.rule_id == "SL-001"));
        assert!(result.findings.iter().any(|f| f.rule_id == "SL-003"));
    }
}
