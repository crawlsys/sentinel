//! Test Relevance Verification
//!
//! Verifies that tests and changes are relevant to the current task.
//! Uses regex detection + optional AI check.

use anyhow::Result;

/// Port for AI-based relevance checking
#[async_trait::async_trait]
pub trait RelevanceChecker: Send + Sync {
    /// Check if changes are relevant to the task
    async fn check_relevance(
        &self,
        changed_files: &[String],
        task_description: &str,
    ) -> Result<RelevanceResult>;
}

/// Result of relevance check
#[derive(Debug, Clone)]
pub struct RelevanceResult {
    /// Whether the changes are relevant
    pub relevant: bool,

    /// Confidence score (0.0 - 1.0)
    pub confidence: f64,

    /// Explanation
    pub reasoning: String,
}

/// Evidence detection patterns (regex-based, before AI check)
pub struct EvidenceDetector {
    /// Patterns that indicate test execution
    pub test_patterns: Vec<regex::Regex>,

    /// Patterns that indicate code review
    pub review_patterns: Vec<regex::Regex>,
}

impl Default for EvidenceDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl EvidenceDetector {
    #[must_use]
    pub fn new() -> Self {
        Self {
            test_patterns: vec![
                regex::Regex::new(r"(?i)\d+\s+(tests?\s+)?(passing|passed)").unwrap(),
                regex::Regex::new(r"(?i)tests?\s+passed").unwrap(),
                regex::Regex::new(r"(?i)npm\s+test|vitest|jest|cargo\s+test").unwrap(),
                regex::Regex::new(r"(?i)coverage.*\d+%").unwrap(),
            ],
            review_patterns: vec![
                regex::Regex::new(r"(?i)code\s+review").unwrap(),
                regex::Regex::new(r"(?i)lgtm|approved|changes\s+requested").unwrap(),
                regex::Regex::new(r"(?i)pr\s+#?\d+").unwrap(),
            ],
        }
    }

    /// Detect test evidence in text
    #[must_use]
    pub fn has_test_evidence(&self, text: &str) -> bool {
        self.test_patterns.iter().any(|p| p.is_match(text))
    }

    /// Detect review evidence in text
    #[must_use]
    pub fn has_review_evidence(&self, text: &str) -> bool {
        self.review_patterns.iter().any(|p| p.is_match(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_test_evidence() {
        let detector = EvidenceDetector::new();
        assert!(detector.has_test_evidence("5 tests passing"));
        assert!(detector.has_test_evidence("Tests passed successfully"));
        assert!(detector.has_test_evidence("npm test completed"));
        assert!(detector.has_test_evidence("Coverage: 85%"));
        assert!(!detector.has_test_evidence("hello world"));
    }

    #[test]
    fn test_detect_review_evidence() {
        let detector = EvidenceDetector::new();
        assert!(detector.has_review_evidence("Code review completed"));
        assert!(detector.has_review_evidence("LGTM, merging"));
        assert!(detector.has_review_evidence("See PR #123"));
        assert!(!detector.has_review_evidence("wrote some code"));
    }
}
