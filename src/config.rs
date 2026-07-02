use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SlopConfig {
    pub ignore_paths: Vec<String>,
    pub rules: BTreeMap<String, RuleConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct RuleConfig {
    pub enabled: Option<bool>,
    pub threshold: Option<usize>,
}

impl Default for SlopConfig {
    fn default() -> Self {
        Self {
            ignore_paths: vec![
                "vendor".into(),
                "generated".into(),
                "dist".into(),
                "build".into(),
                "target".into(),
                "node_modules".into(),
            ],
            rules: BTreeMap::new(),
        }
    }
}

impl SlopConfig {
    pub fn load(repo_path: &Path) -> Result<Self> {
        let path = repo_path.join(".sloplens.yml");
        match fs::read_to_string(&path) {
            Ok(raw) => {
                let config = serde_yaml::from_str(&raw)
                    .with_context(|| format!("failed to parse {}", path.display()))?;
                Ok(with_default_ignores(config))
            }
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    pub fn rule_enabled(&self, rule_id: &str) -> bool {
        self.rules
            .get(rule_id)
            .and_then(|rule| rule.enabled)
            .unwrap_or(true)
    }

    pub fn sl003_threshold(&self) -> usize {
        self.rules
            .get("SL-003")
            .and_then(|rule| rule.threshold)
            .unwrap_or(80)
    }

    pub fn is_ignored(&self, relative_path: &Path) -> bool {
        let relative = normalize(relative_path);
        self.ignore_paths.iter().any(|pattern| {
            let pattern = pattern.trim().trim_matches('/');
            if pattern.is_empty() {
                return false;
            }
            let pattern = pattern.replace('\\', "/");
            if pattern.contains('*') {
                matches_glob(&relative, &pattern)
            } else if pattern.contains('/') {
                relative == pattern || relative.starts_with(&format!("{pattern}/"))
            } else {
                relative.split('/').any(|component| component == pattern)
            }
        })
    }
}

fn with_default_ignores(mut config: SlopConfig) -> SlopConfig {
    let mut ignore_paths = SlopConfig::default().ignore_paths;
    for path in config.ignore_paths {
        if !ignore_paths.contains(&path) {
            ignore_paths.push(path);
        }
    }
    config.ignore_paths = ignore_paths;
    config
}

fn normalize(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn matches_glob(path: &str, pattern: &str) -> bool {
    let path_parts = path.split('/').collect::<Vec<_>>();
    let pattern_parts = pattern.split('/').collect::<Vec<_>>();
    matches_glob_parts(&path_parts, &pattern_parts)
}

fn matches_glob_parts(path: &[&str], pattern: &[&str]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => {
            matches_glob_parts(path, rest)
                || (!path.is_empty() && matches_glob_parts(&path[1..], pattern))
        }
        Some((head, rest)) => {
            !path.is_empty()
                && matches_segment(path[0], head)
                && matches_glob_parts(&path[1..], rest)
        }
    }
}

fn matches_segment(value: &str, pattern: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return value == pattern;
    }
    let mut remainder = value;
    let starts_with_wildcard = pattern.starts_with('*');
    let ends_with_wildcard = pattern.ends_with('*');
    let parts = pattern
        .split('*')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return true;
    }
    if !starts_with_wildcard && !remainder.starts_with(parts[0]) {
        return false;
    }
    for part in &parts {
        let Some(index) = remainder.find(part) else {
            return false;
        };
        remainder = &remainder[index + part.len()..];
    }
    ends_with_wildcard || remainder.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir() -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("slop-lens-config-test-{stamp}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn missing_config_uses_defaults() {
        let dir = temp_dir();
        let config = SlopConfig::load(&dir).unwrap();
        assert!(config.is_ignored(Path::new("vendor/generated.rs")));
        assert!(config.rule_enabled("SL-001"));
        assert_eq!(config.sl003_threshold(), 80);
    }

    #[test]
    fn parses_ignore_paths_and_rule_settings() {
        let dir = temp_dir();
        fs::write(
            dir.join(".sloplens.yml"),
            "ignore_paths:\n  - fixtures/**\nrules:\n  SL-001:\n    enabled: false\n  SL-003:\n    threshold: 20\n",
        )
        .unwrap();
        let config = SlopConfig::load(&dir).unwrap();
        assert!(config.is_ignored(Path::new("fixtures/sample.py")));
        assert!(config.is_ignored(Path::new("vendor/lib.rs")));
        assert!(!config.rule_enabled("SL-001"));
        assert_eq!(config.sl003_threshold(), 20);
    }
}
