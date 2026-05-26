//! Evidence collection and types
//!
//! Evidence is what gets hashed into the proof chain.
//! It captures what Claude actually did during a phase.

use serde::{Deserialize, Serialize};

/// Evidence collected during a skill phase
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Evidence {
    /// Tool calls Claude made during this phase
    pub tool_calls: Vec<ToolCallEvidence>,

    /// Tool results returned during this phase
    pub tool_results: Vec<ToolResultEvidence>,

    /// Files changed (from git diff)
    pub files_changed: Vec<String>,

    /// Whether the phase .md file was `Read()`
    pub phase_file_read: bool,

    /// Phase-specific custom evidence (e.g., test results, PR URL, Linear issue ID)
    #[serde(default)]
    pub custom: serde_json::Value,

    /// Step IDs completed during this phase
    #[serde(default)]
    pub steps_completed: Vec<String>,

    /// Step IDs skipped during this phase
    #[serde(default)]
    pub steps_skipped: Vec<String>,
}

/// A tool call Claude made
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallEvidence {
    /// Tool name (e.g., "Bash", "Read", "`mcp__linear__get_issue`")
    pub tool: String,

    /// Key arguments (truncated for hashing efficiency)
    pub args_summary: String,

    /// Timestamp
    pub timestamp: String,
}

/// A tool result that was returned
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultEvidence {
    /// Tool name
    pub tool: String,

    /// First 500 chars of result (enough for hashing, not full content)
    pub result_summary: String,

    /// Whether the tool call succeeded
    pub success: bool,
}

/// An individual evidence entry (used during collection before finalization)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EvidenceEntry {
    ToolCall(ToolCallEvidence),
    ToolResult(ToolResultEvidence),
    FileChanged(String),
    PhaseFileRead,
    Custom(String, serde_json::Value),
}

/// Evidence collector — accumulates entries during a phase, then finalizes
#[derive(Debug, Clone, Default)]
pub struct EvidenceCollector {
    entries: Vec<EvidenceEntry>,
}

/// **Attack #171 fix**: Maximum evidence entries per phase.
/// Prevents unbounded memory/disk growth from tool call loops.
/// 10,000 entries is generous for any legitimate phase (most have <100).
const MAX_EVIDENCE_ENTRIES: usize = 10_000;

impl EvidenceCollector {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if we've hit the evidence entry cap. If so, log a warning once.
    fn check_capacity(&self) -> bool {
        if self.entries.len() >= MAX_EVIDENCE_ENTRIES {
            if self.entries.len() == MAX_EVIDENCE_ENTRIES {
                eprintln!(
                    "[sentinel] WARNING: Evidence collector hit max entries ({MAX_EVIDENCE_ENTRIES}). \
                     Further evidence will be dropped. This may indicate a tool call loop."
                );
            }
            return false;
        }
        true
    }

    /// Record a tool call
    pub fn record_tool_call(&mut self, tool: &str, args_summary: &str) {
        if !self.check_capacity() {
            return;
        }
        self.entries.push(EvidenceEntry::ToolCall(ToolCallEvidence {
            tool: tool.to_string(),
            args_summary: args_summary.chars().take(200).collect(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        }));
    }

    /// Record a tool result
    pub fn record_tool_result(&mut self, tool: &str, result: &str, success: bool) {
        if !self.check_capacity() {
            return;
        }
        self.entries
            .push(EvidenceEntry::ToolResult(ToolResultEvidence {
                tool: tool.to_string(),
                result_summary: result.chars().take(500).collect(),
                success,
            }));
    }

    /// Record a file change
    pub fn record_file_changed(&mut self, path: &str) {
        if !self.check_capacity() {
            return;
        }
        self.entries
            .push(EvidenceEntry::FileChanged(path.to_string()));
    }

    /// Record that the phase file was read
    pub fn record_phase_file_read(&mut self) {
        if !self.check_capacity() {
            return;
        }
        self.entries.push(EvidenceEntry::PhaseFileRead);
    }

    /// Record custom evidence.
    /// **Attack #134 fix**: Limits key to 128 chars and serialized value to 8KB
    /// to prevent memory/disk exhaustion via evidence bloat.
    pub fn record_custom(&mut self, key: &str, value: serde_json::Value) {
        if !self.check_capacity() {
            return;
        }
        let key = if key.len() > 128 { &key[..128] } else { key };
        // Cap serialized value at 8KB
        let serialized_len = serde_json::to_string(&value).map_or(0, |s| s.len());
        if serialized_len > 8192 {
            eprintln!(
                "[sentinel] WARNING: Custom evidence '{key}' exceeds 8KB ({serialized_len} bytes), truncating to null"
            );
            self.entries.push(EvidenceEntry::Custom(
                key.to_string(),
                serde_json::json!("(truncated — exceeded 8KB limit)"),
            ));
        } else {
            self.entries
                .push(EvidenceEntry::Custom(key.to_string(), value));
        }
    }

    /// Finalize into an Evidence struct
    #[must_use]
    pub fn finalize(self) -> Evidence {
        let mut evidence = Evidence::default();
        let mut custom_map = serde_json::Map::new();

        for entry in self.entries {
            match entry {
                EvidenceEntry::ToolCall(tc) => evidence.tool_calls.push(tc),
                EvidenceEntry::ToolResult(tr) => evidence.tool_results.push(tr),
                EvidenceEntry::FileChanged(f) => evidence.files_changed.push(f),
                EvidenceEntry::PhaseFileRead => evidence.phase_file_read = true,
                EvidenceEntry::Custom(k, v) => {
                    custom_map.insert(k, v);
                }
            }
        }

        if !custom_map.is_empty() {
            evidence.custom = serde_json::Value::Object(custom_map);
        }

        evidence
    }

    /// Number of entries collected so far
    #[must_use]
    pub const fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether any evidence has been collected
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_collector_finalize() {
        let mut collector = EvidenceCollector::new();
        collector.record_tool_call("Bash", "npm test");
        collector.record_tool_result("Bash", "5 passing", true);
        collector.record_file_changed("src/main.rs");
        collector.record_phase_file_read();
        collector.record_custom("pr_url", serde_json::json!("https://github.com/..."));

        let evidence = collector.finalize();
        assert_eq!(evidence.tool_calls.len(), 1);
        assert_eq!(evidence.tool_results.len(), 1);
        assert_eq!(evidence.files_changed.len(), 1);
        assert!(evidence.phase_file_read);
        assert!(evidence.custom.is_object());
    }

    #[test]
    fn test_empty_collector() {
        let collector = EvidenceCollector::new();
        assert!(collector.is_empty());
        let evidence = collector.finalize();
        assert!(evidence.tool_calls.is_empty());
        assert!(!evidence.phase_file_read);
    }

    #[test]
    fn test_evidence_entry_cap() {
        let mut collector = EvidenceCollector::new();
        for i in 0..MAX_EVIDENCE_ENTRIES + 100 {
            collector.record_tool_call("Bash", &format!("cmd_{i}"));
        }
        // Should be capped at MAX_EVIDENCE_ENTRIES
        assert_eq!(collector.len(), MAX_EVIDENCE_ENTRIES);
        let evidence = collector.finalize();
        assert_eq!(evidence.tool_calls.len(), MAX_EVIDENCE_ENTRIES);
    }

    #[test]
    fn test_args_truncation() {
        let mut collector = EvidenceCollector::new();
        let long_args = "x".repeat(500);
        collector.record_tool_call("Bash", &long_args);
        let evidence = collector.finalize();
        assert_eq!(evidence.tool_calls[0].args_summary.len(), 200);
    }
}
