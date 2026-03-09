//! Skill Router Hook
//!
//! Detects which skill matches the user's message.
//! Regex pre-match runs first, AI classifier fallback.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use sentinel_domain::routing::RegexRouter;
use std::fs;
use std::io::Write;

/// Load Linear team keys from the cache file written at session start.
/// Falls back to a hardcoded set if the cache doesn't exist yet.
fn load_linear_team_keys() -> Vec<String> {
    let cache_path = dirs::home_dir()
        .unwrap_or_default()
        .join(".claude")
        .join("sentinel")
        .join("linear-teams.json");

    if let Ok(content) = fs::read_to_string(&cache_path) {
        if let Ok(keys) = serde_json::from_str::<Vec<String>>(&content) {
            if !keys.is_empty() {
                return keys;
            }
        }
    }

    // Fallback — hardcoded keys (updated 2026-03-09 from Linear API)
    vec![
        "FPCRM", "FPFIELD", "FPROUTE", "GS", "COR", "LEG", "TRB",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Build the default regex router with all skill patterns
pub fn default_router() -> RegexRouter {
    let mut router = RegexRouter::new();

    // Linear — highest priority (issue IDs are unambiguous)
    // Team keys loaded dynamically from ~/.claude/sentinel/linear-teams.json
    // (written by session_init hook via Linear API, falls back to hardcoded)
    let team_keys = load_linear_team_keys();
    let team_pattern = format!(
        r"(?i)\b({})-\d+\b",
        team_keys
            .iter()
            .map(|k| regex::escape(k))
            .collect::<Vec<_>>()
            .join("|")
    );
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "linear",
        vec![
            &team_pattern,
            r"(?i)\blinear\b",
            r"(?i)\b(pick\s+up|claim|assign|work\s+on)\b.*\b(issue|ticket)\b",
            r"(?i)\b(in\s+review|assign\s+to\s+qa|qa\s+handoff|mark\s+complete)\b",
            r"(?i)\b(sprint|cycle|milestone|backlog|triage|initiative|roadmap)\b",
        ],
        100,
    ) {
        router.add_rule(rule);
    }

    // Git — commit, push, branch, PR
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "git",
        vec![
            r"(?i)^/commit\b",
            r"(?i)\b(commit|push|pull|merge|rebase|cherry.pick)\s+(this|these|my|the|all)\b",
            r"(?i)\bcreate\s+(a\s+)?(branch|pr|pull\s+request)\b",
        ],
        90,
    ) {
        router.add_rule(rule);
    }

    // Test
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "test",
        vec![
            r"(?i)^/test\b",
            r"(?i)\b(run|write|add|fix)\s+(the\s+)?tests?\b",
            r"(?i)\b(vitest|jest|pytest|cargo\s+test)\b",
            r"(?i)\btest\s+coverage\b",
        ],
        85,
    ) {
        router.add_rule(rule);
    }

    // Review
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "review",
        vec![
            r"(?i)^/review\b",
            r"(?i)\b(code\s+)?review\s+(this|these|my|the|changes)\b",
        ],
        85,
    ) {
        router.add_rule(rule);
    }

    // Receiving Code Review — handle incoming review feedback
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "receiving-code-review",
        vec![
            r"(?i)\b(address|fix|handle|respond\s+to)\s+(the\s+)?(review|PR)\s+(comment|feedback|suggestion)s?\b",
            r"(?i)\breviewer\s+said\b",
            r"(?i)\b(review|PR)\s+(feedback|comments)\s+(from|on)\b",
        ],
        88,
    ) {
        router.add_rule(rule);
    }

    // Debug
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "debug",
        vec![
            r"(?i)^/debug\b",
            r"(?i)\b(debug|troubleshoot|diagnose|investigate)\s+(this|the|why)\b",
            r"(?i)\bwhy\s+is\s+(this|it)\s+(failing|broken|not\s+working)\b",
        ],
        80,
    ) {
        router.add_rule(rule);
    }

    // Explore
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "explore",
        vec![
            r"(?i)^/explore\b",
            r"(?i)\b(explore|understand|find|search|where\s+is|how\s+does)\b.*\b(code|codebase|repo)\b",
        ],
        75,
    ) {
        router.add_rule(rule);
    }

    // Plan
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "plan",
        vec![
            r"(?i)^/plan\b",
            r"(?i)\b(plan|design|architect)\s+(the|this|how|an?)\b",
        ],
        75,
    ) {
        router.add_rule(rule);
    }

    // Deploy
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "deploy",
        vec![
            r"(?i)^/deploy\b",
            r"(?i)\bdeploy\s+(to|this|the)\b",
            r"(?i)\b(release|ship\s+it|cut\s+release)\b",
        ],
        80,
    ) {
        router.add_rule(rule);
    }

    // Execute
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "execute",
        vec![
            r"(?i)\b(execute|implement|build\s+it|do\s+it|make\s+it)\b",
            r"(?i)\blets?\s+build\b",
        ],
        70,
    ) {
        router.add_rule(rule);
    }

    // Security
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "security",
        vec![r"(?i)\b(security|vulnerability|CVE|OWASP|penetration)\b"],
        80,
    ) {
        router.add_rule(rule);
    }

    // Refactor
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "refactor",
        vec![r"(?i)\b(refactor|restructure|clean\s+up)\s+(this|the|code)\b"],
        75,
    ) {
        router.add_rule(rule);
    }

    // Document
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "document",
        vec![r"(?i)\b(document|write\s+docs|add\s+documentation)\b"],
        70,
    ) {
        router.add_rule(rule);
    }

    // DDD/Hexagonal
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "ddd-hexagonal",
        vec![
            r"(?i)\b(ddd|domain.driven|hexagonal|ports?.and.adapters|clean\s+arch)\b",
        ],
        85,
    ) {
        router.add_rule(rule);
    }

    // Steel
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "steel",
        vec![
            r"(?i)\b(steel|cloud\s+browser|scrape|headless\s+browser)\b",
            r"(?i)\bsteel\s+(session|navigate|screenshot)\b",
        ],
        80,
    ) {
        router.add_rule(rule);
    }

    // Doppler
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "doppler",
        vec![
            r"(?i)\bdoppler\b",
            r"(?i)\b(secrets?|env\s+vars?)\s+(management|rotation)\b",
        ],
        80,
    ) {
        router.add_rule(rule);
    }

    // Cerebras — fast inference, ZAI-GLM, Qwen models
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "cerebras",
        vec![
            r"(?i)\bcerebras\b",
            r"(?i)\bfast\s+inference\b",
            r"(?i)\bzai[-\s]?glm\b",
            r"(?i)\bqwen[-\s]?3\b",
        ],
        60,
    ) {
        router.add_rule(rule);
    }

    // Internet — network/router management (ATT/Netgear)
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "internet",
        vec![
            r"(?i)\b(my\s+)?router\b",
            r"(?i)\bnetgear\b",
            r"(?i)\batt\s+gateway\b",
            r"(?i)\bport\s+forwarding\b",
            r"(?i)\bip\s+passthrough\b",
            r"(?i)\bnetwork\s+health\b",
            r"(?i)\bwan\s+bounce\b",
            r"(?i)\bdhcp\b",
        ],
        60,
    ) {
        router.add_rule(rule);
    }

    router
}

/// Write temp state files so skill_telemetry can track the execution.
/// This mirrors the behavior of the legacy JS skill-router.js.
fn write_telemetry_state(skill: &str, run_id: &str) {
    let tmp = std::env::temp_dir();

    // Current skill name
    let _ = fs::write(tmp.join("claude-current-skill"), skill);

    // Run ID for correlation
    let _ = fs::write(tmp.join("claude-skill-run-id"), run_id);

    // Start timestamp (epoch ms) for duration calculation
    let now_ms = chrono::Utc::now().timestamp_millis();
    let _ = fs::write(tmp.join("claude-skill-start-time"), now_ms.to_string());
}

/// Append a routing entry to metrics/routing.jsonl
fn write_routing_entry(skill: &str, run_id: &str, input: &HookInput, prompt: &str) {
    let metrics_dir = match dirs::home_dir() {
        Some(h) => h.join(".claude").join("metrics"),
        None => return,
    };
    let _ = fs::create_dir_all(&metrics_dir);

    let session_id = input.session_id.as_deref().unwrap_or("unknown");
    let cwd = input.cwd.as_deref().unwrap_or(".");
    let ts = chrono::Utc::now().to_rfc3339();

    // Truncate prompt for logging (first 100 chars)
    let prompt_short: String = prompt.chars().take(100).collect();

    let entry = serde_json::json!({
        "run_id": run_id,
        "session_id": session_id,
        "event": "skill_routed",
        "skill": skill,
        "source": "regex",
        "status": "started",
        "cwd": cwd,
        "prompt": prompt_short,
        "ts": ts,
    });

    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(metrics_dir.join("routing.jsonl"))
    {
        let _ = writeln!(file, "{}", serde_json::to_string(&entry).unwrap_or_default());
    }
}

/// Process a skill-router hook event
pub fn process(input: &HookInput, router: &RegexRouter) -> HookOutput {
    let prompt = match &input.prompt {
        Some(p) => p,
        None => return HookOutput::allow(),
    };

    match router.route(prompt) {
        Some(m) => {
            // Generate a unique run ID
            let now_ms = chrono::Utc::now().timestamp_millis();
            let pid = std::process::id();
            let run_id = format!("{}-{}", now_ms, pid % 100000);

            // Write temp state files for skill_telemetry to read on Stop
            write_telemetry_state(&m.skill, &run_id);

            // Append routing entry to metrics
            write_routing_entry(&m.skill, &run_id, input, prompt);

            let skill_path = format!("~/.claude/skills/{}/SKILL.md", m.skill);
            let context = format!(
                "[Skill Router] Detected skill: {}. \
                 MANDATORY: You MUST Read(\"{}\") BEFORE responding. \
                 This is a non-negotiable requirement.",
                m.skill, skill_path
            );
            HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
        }
        None => {
            // Clear temp state so telemetry records "none" accurately
            let tmp = std::env::temp_dir();
            let _ = fs::remove_file(tmp.join("claude-current-skill"));
            let _ = fs::remove_file(tmp.join("claude-skill-run-id"));
            let _ = fs::remove_file(tmp.join("claude-skill-start-time"));

            // Always report router status so user knows it fired
            HookOutput::inject_context(
                HookEvent::UserPromptSubmit,
                "[Skill Router] No skill matched — general conversation mode.".to_string(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_router_has_rules() {
        let router = default_router();
        assert!(router.rule_count() > 10, "Expected 10+ routing rules");
    }

    #[test]
    fn test_linear_issue_id_routing() {
        let router = default_router();
        // Test keys that are always present (hardcoded fallback + cache)
        // The exact set depends on the runtime cache, so test the keys
        // that load_linear_team_keys() actually returns
        let keys = super::load_linear_team_keys();
        for prefix in &keys {
            let input = HookInput {
                prompt: Some(format!("Pick up {prefix}-123")),
                ..Default::default()
            };
            let output = process(&input, &router);
            let ctx = output.hook_specific_output.unwrap();
            assert!(
                ctx.additional_context.contains("linear"),
                "{prefix}-123 should route to linear"
            );
        }
    }

    #[test]
    fn test_fpcrm_issue_id_routing() {
        let router = default_router();
        let input = HookInput {
            prompt: Some("Pick up FPCRM-42".to_string()),
            ..Default::default()
        };
        let output = process(&input, &router);
        let ctx = output.hook_specific_output.unwrap();
        assert!(ctx.additional_context.contains("linear"));
    }

    #[test]
    fn test_git_commit_routing() {
        let router = default_router();
        let input = HookInput {
            prompt: Some("/commit".to_string()),
            ..Default::default()
        };
        let output = process(&input, &router);
        let ctx = output.hook_specific_output.unwrap();
        assert!(ctx.additional_context.contains("git"));
    }

    #[test]
    fn test_no_match_returns_context() {
        let router = default_router();
        let input = HookInput {
            prompt: Some("hello world, how are you?".to_string()),
            ..Default::default()
        };
        let output = process(&input, &router);
        // No-match now injects "general conversation mode" context
        let ctx = output.hook_specific_output.unwrap();
        assert!(ctx.additional_context.contains("No skill matched"));
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_no_prompt_returns_allow() {
        let router = default_router();
        let input = HookInput::default();
        let output = process(&input, &router);
        // No prompt at all → plain allow (no context injection)
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_test_skill_routing() {
        let router = default_router();
        let input = HookInput {
            prompt: Some("run the tests".to_string()),
            ..Default::default()
        };
        let output = process(&input, &router);
        let ctx = output.hook_specific_output.unwrap();
        assert!(ctx.additional_context.contains("test"));
    }

    #[test]
    fn test_ddd_routing() {
        let router = default_router();
        let input = HookInput {
            prompt: Some("use hexagonal architecture for this".to_string()),
            ..Default::default()
        };
        let output = process(&input, &router);
        let ctx = output.hook_specific_output.unwrap();
        assert!(ctx.additional_context.contains("ddd-hexagonal"));
    }

    #[test]
    fn test_deploy_routing() {
        let router = default_router();
        let input = HookInput {
            prompt: Some("deploy to production".to_string()),
            ..Default::default()
        };
        let output = process(&input, &router);
        let ctx = output.hook_specific_output.unwrap();
        assert!(ctx.additional_context.contains("deploy"));
    }

    #[test]
    fn test_security_routing() {
        let router = default_router();
        let input = HookInput {
            prompt: Some("check for CVE vulnerabilities".to_string()),
            ..Default::default()
        };
        let output = process(&input, &router);
        let ctx = output.hook_specific_output.unwrap();
        assert!(ctx.additional_context.contains("security"));
    }

    #[test]
    fn test_cerebras_routing() {
        let router = default_router();
        let input = HookInput {
            prompt: Some("run this with cerebras fast inference".to_string()),
            ..Default::default()
        };
        let output = process(&input, &router);
        let ctx = output.hook_specific_output.unwrap();
        assert!(ctx.additional_context.contains("cerebras"));
    }

    #[test]
    fn test_cerebras_qwen_routing() {
        let router = default_router();
        let input = HookInput {
            prompt: Some("use qwen-3 for this task".to_string()),
            ..Default::default()
        };
        let output = process(&input, &router);
        let ctx = output.hook_specific_output.unwrap();
        assert!(ctx.additional_context.contains("cerebras"));
    }

    #[test]
    fn test_internet_netgear_routing() {
        let router = default_router();
        let input = HookInput {
            prompt: Some("check my netgear wifi clients".to_string()),
            ..Default::default()
        };
        let output = process(&input, &router);
        let ctx = output.hook_specific_output.unwrap();
        assert!(ctx.additional_context.contains("internet"));
    }

    #[test]
    fn test_internet_port_forwarding_routing() {
        let router = default_router();
        let input = HookInput {
            prompt: Some("set up port forwarding on my router".to_string()),
            ..Default::default()
        };
        let output = process(&input, &router);
        let ctx = output.hook_specific_output.unwrap();
        assert!(ctx.additional_context.contains("internet"));
    }

    #[test]
    fn test_internet_dhcp_routing() {
        let router = default_router();
        let input = HookInput {
            prompt: Some("show me the DHCP leases".to_string()),
            ..Default::default()
        };
        let output = process(&input, &router);
        let ctx = output.hook_specific_output.unwrap();
        assert!(ctx.additional_context.contains("internet"));
    }
}
