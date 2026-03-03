//! Skill Router Hook
//!
//! Detects which skill matches the user's message.
//! Regex pre-match runs first, AI classifier fallback.

use sentinel_domain::events::{HookEvent, HookInput, HookOutput};
use sentinel_domain::routing::RegexRouter;

/// Build the default regex router with all skill patterns
pub fn default_router() -> RegexRouter {
    let mut router = RegexRouter::new();

    // Linear — highest priority (issue IDs are unambiguous)
    if let Ok(rule) = sentinel_domain::routing::RoutingRule::new(
        "linear",
        vec![
            r"(?i)\b(fir|cor|per|eng|des|ops|sec|dat|api|mob|inf|qat|rel)-\d+\b",
            r"(?i)\blinear\s+(issue|ticket|bug|feature|task)\b",
            r"(?i)\b(pick\s+up|claim|assign|work\s+on)\b.*\b(issue|ticket)\b",
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

    router
}

/// Process a skill-router hook event
pub fn process(input: &HookInput, router: &RegexRouter) -> HookOutput {
    let prompt = match &input.prompt {
        Some(p) => p,
        None => return HookOutput::allow(),
    };

    match router.route(prompt) {
        Some(m) => {
            let skill_path = format!("~/.claude/skills/{}/SKILL.md", m.skill);
            let context = format!(
                "[Skill Router] Detected skill: {}. \
                 MANDATORY: You MUST Read(\"{}\") BEFORE responding. \
                 This is a non-negotiable requirement.",
                m.skill, skill_path
            );
            HookOutput::inject_context(HookEvent::UserPromptSubmit, context)
        }
        None => HookOutput::allow(),
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
        let input = HookInput {
            prompt: Some("Pick up FIR-123".to_string()),
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
    fn test_no_match_returns_allow() {
        let router = default_router();
        let input = HookInput {
            prompt: Some("hello world, how are you?".to_string()),
            ..Default::default()
        };
        let output = process(&input, &router);
        assert!(output.hook_specific_output.is_none());
        assert!(output.blocked.is_none());
    }

    #[test]
    fn test_no_prompt_returns_allow() {
        let router = default_router();
        let input = HookInput::default();
        let output = process(&input, &router);
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
}
