//! Regex Router
//!
//! Fast pre-match for skill routing. Regex patterns run first (~0.1ms),
//! then AI classifier runs only if no strong regex match.

use regex::Regex;
use serde::{Deserialize, Serialize};

/// A skill routing rule (regex-based pre-match)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingRule {
    /// Skill name this rule matches
    pub skill: String,

    /// Regex patterns that trigger this skill
    #[serde(skip)]
    pub patterns: Vec<Regex>,

    /// Raw pattern strings (for serialization)
    pub pattern_strings: Vec<String>,

    /// Priority (higher = checked first)
    #[serde(default)]
    pub priority: i32,
}

impl RoutingRule {
    /// Create a new routing rule with compiled patterns
    pub fn new(
        skill: impl Into<String>,
        patterns: Vec<&str>,
        priority: i32,
    ) -> Result<Self, regex::Error> {
        let pattern_strings: Vec<String> = patterns.iter().map(|p| (*p).to_string()).collect();
        let compiled: Vec<Regex> = patterns
            .iter()
            .map(|p| Regex::new(p))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            skill: skill.into(),
            patterns: compiled,
            pattern_strings,
            priority,
        })
    }

    /// Check if a message matches any pattern
    #[must_use]
    pub fn matches(&self, message: &str) -> bool {
        self.patterns.iter().any(|p| p.is_match(message))
    }
}

/// The regex router — fast first-pass before AI classifier
#[derive(Debug, Default)]
pub struct RegexRouter {
    rules: Vec<RoutingRule>,
}

/// Result of regex routing
#[derive(Debug, Clone)]
pub struct RoutingMatch {
    /// Matched skill name
    pub skill: String,

    /// Priority of the match
    pub priority: i32,

    /// Whether this is a strong match (high priority)
    pub strong: bool,
}

impl RegexRouter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a routing rule
    pub fn add_rule(&mut self, rule: RoutingRule) {
        self.rules.push(rule);
        // Keep sorted by priority (highest first)
        self.rules.sort_by_key(|r| std::cmp::Reverse(r.priority));
    }

    /// Route a message — returns best match or None
    #[must_use]
    pub fn route(&self, message: &str) -> Option<RoutingMatch> {
        let lower = message.to_lowercase();
        for rule in &self.rules {
            if rule.matches(&lower) {
                return Some(RoutingMatch {
                    skill: rule.skill.clone(),
                    priority: rule.priority,
                    strong: rule.priority >= 80,
                });
            }
        }
        None
    }

    /// Route a message — returns all matches sorted by priority
    #[must_use]
    pub fn route_all(&self, message: &str) -> Vec<RoutingMatch> {
        let lower = message.to_lowercase();
        self.rules
            .iter()
            .filter(|r| r.matches(&lower))
            .map(|r| RoutingMatch {
                skill: r.skill.clone(),
                priority: r.priority,
                strong: r.priority >= 80,
            })
            .collect()
    }

    /// Number of registered rules
    #[must_use]
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_router() -> RegexRouter {
        let mut router = RegexRouter::new();
        router.add_rule(
            RoutingRule::new(
                "linear",
                vec![r"(?i)\b(fir|cor|per)-\d+\b", r"(?i)\blinear\b"],
                90,
            )
            .unwrap(),
        );
        router.add_rule(
            RoutingRule::new(
                "git",
                vec![r"(?i)\b(commit|push|branch|merge|rebase)\b"],
                80,
            )
            .unwrap(),
        );
        router.add_rule(
            RoutingRule::new("test", vec![r"(?i)\b(test|jest|vitest|coverage)\b"], 70).unwrap(),
        );
        router
    }

    #[test]
    fn test_linear_by_issue_id() {
        let router = test_router();
        let m = router.route("Pick up FIR-123").unwrap();
        assert_eq!(m.skill, "linear");
        assert!(m.strong);
    }

    #[test]
    fn test_linear_by_keyword() {
        let router = test_router();
        let m = router.route("Check my Linear issues").unwrap();
        assert_eq!(m.skill, "linear");
    }

    #[test]
    fn test_git_match() {
        let router = test_router();
        let m = router.route("commit these changes").unwrap();
        assert_eq!(m.skill, "git");
        assert!(m.strong);
    }

    #[test]
    fn test_no_match() {
        let router = test_router();
        assert!(router.route("hello world").is_none());
    }

    #[test]
    fn test_priority_order() {
        let router = test_router();
        // "linear" has higher priority than "git"
        let matches = router.route_all("commit changes for FIR-123");
        assert!(!matches.is_empty());
        assert_eq!(matches[0].skill, "linear");
    }

    #[test]
    fn test_case_insensitive() {
        let router = test_router();
        assert!(router.route("RUN TEST").is_some());
        assert_eq!(router.route("RUN TEST").unwrap().skill, "test");
        // "tests" also matches via \b boundary on 't' start
        assert!(router.route("run vitest").is_some());
    }
}
