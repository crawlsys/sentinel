//! GitHub webhook decoders.
//!
//! GitHub delivers the event type in the `X-GitHub-Event` header, not the
//! payload. The caller must pass that through (pipeline: Hookdeck preserves
//! inbound headers → channel bridge extracts it → passes as `event_type`).
//!
//! Supported events:
//!
//! - `pull_request`: opened, closed (w/ merge detection), reopened, edited,
//!   synchronize, `ready_for_review`, labeled
//! - `pull_request_review`: submitted (`approved/changes_requested/commented`),
//!   edited, dismissed
//! - `issue_comment`: created (PR review-comment body), edited, deleted
//! - `pull_request_review_comment`: inline file-level PR review comments
//! - `check_run`: completed (`success/failure/cancelled/timed_out`), rerequested
//! - `check_suite`: completed
//! - `push`: commits to a ref (detects branch, count, head sha)
//! - `workflow_run`: completed (conclusion surfaced)
//! - `issues`: opened, closed, reopened, labeled

use serde_json::Value;

use super::{truncate_inline, Decoded};

pub fn decode(event_type: Option<&str>, body: &Value) -> Option<Decoded> {
    // Prefer the explicit X-GitHub-Event header; otherwise guess from payload.
    let et = event_type
        .map(str::to_string)
        .or_else(|| infer_event_type(body))?;

    let action = body.get("action").and_then(Value::as_str);

    let summary = match et.as_str() {
        "pull_request" => decode_pull_request(action, body),
        "pull_request_review" => decode_pr_review(action, body),
        "pull_request_review_comment" => decode_pr_review_comment(action, body),
        "issue_comment" => decode_issue_comment(action, body),
        "issues" => decode_issues(action, body),
        "check_run" => decode_check_run(body),
        "check_suite" => decode_check_suite(body),
        "push" => decode_push(body),
        "workflow_run" => decode_workflow_run(body),
        _ => None,
    };

    summary.map(|s| Decoded::new(format!("[GITHUB] {s}"), body))
}

fn infer_event_type(body: &Value) -> Option<String> {
    if body.get("check_run").is_some() {
        return Some("check_run".into());
    }
    if body.get("check_suite").is_some() {
        return Some("check_suite".into());
    }
    if body.get("pull_request").is_some() {
        return Some("pull_request".into());
    }
    if body.get("workflow_run").is_some() {
        return Some("workflow_run".into());
    }
    if body.get("commits").is_some() && body.get("ref").is_some() {
        return Some("push".into());
    }
    None
}

fn repo_full_name(body: &Value) -> String {
    body.pointer("/repository/full_name")
        .and_then(Value::as_str)
        .unwrap_or("<repo>")
        .to_string()
}

fn sender_login(body: &Value) -> Option<String> {
    body.pointer("/sender/login")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn decode_pull_request(action: Option<&str>, body: &Value) -> Option<String> {
    let pr = body.get("pull_request")?;
    let number = pr.get("number").and_then(Value::as_u64).unwrap_or(0);
    let title = pr
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("(untitled)");
    let repo = repo_full_name(body);
    let sender = sender_login(body);

    let merged = pr.get("merged").and_then(Value::as_bool).unwrap_or(false);

    match action {
        Some("closed") if merged => {
            let sha = pr
                .get("merge_commit_sha")
                .and_then(Value::as_str)
                .map_or_else(|| "?".to_string(), short_sha);
            let base_ref = pr
                .pointer("/base/ref")
                .and_then(Value::as_str)
                .unwrap_or("main");
            match sender {
                Some(u) => Some(format!(
                    "PR #{number} merged to {base_ref} by @{u} (sha {sha}) in {repo}"
                )),
                None => Some(format!(
                    "PR #{number} merged to {base_ref} (sha {sha}) in {repo}"
                )),
            }
        }
        Some("closed") => Some(format!(
            "PR #{number} closed (no merge) in {repo}: {}",
            truncate_inline(title, 60)
        )),
        Some("opened") => Some(format!(
            "PR #{number} opened in {repo}: {}{}",
            truncate_inline(title, 70),
            sender.map(|u| format!(" (by @{u})")).unwrap_or_default()
        )),
        Some("reopened") => Some(format!("PR #{number} reopened in {repo}")),
        Some("ready_for_review") => Some(format!("PR #{number} marked ready for review in {repo}")),
        Some("labeled") => {
            let label = body
                .pointer("/label/name")
                .and_then(Value::as_str)
                .unwrap_or("<label>");
            Some(format!("PR #{number} labeled \"{label}\" in {repo}"))
        }
        Some("synchronize") => {
            let sha = pr
                .pointer("/head/sha")
                .and_then(Value::as_str)
                .map_or_else(|| "?".to_string(), short_sha);
            Some(format!("PR #{number} updated (new head {sha}) in {repo}"))
        }
        Some(other) => Some(format!(
            "PR #{number} {other} in {repo}: {}",
            truncate_inline(title, 60)
        )),
        None => Some(format!("PR #{number} event in {repo}")),
    }
}

fn decode_pr_review(action: Option<&str>, body: &Value) -> Option<String> {
    let number = body
        .pointer("/pull_request/number")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let repo = repo_full_name(body);
    let reviewer = body.pointer("/review/user/login").and_then(Value::as_str);
    let state = body
        .pointer("/review/state")
        .and_then(Value::as_str)
        .unwrap_or("submitted");

    let verb = match (action, state) {
        (Some("submitted"), "approved") => "approved",
        (Some("submitted"), "changes_requested") => "requested changes on",
        (Some("submitted"), "commented") => "commented on",
        (Some("dismissed"), _) => "review dismissed on",
        (Some("edited"), _) => "edited a review on",
        _ => "reviewed",
    };

    match reviewer {
        Some(user) => Some(format!("@{user} {verb} PR #{number} in {repo}")),
        None => Some(format!("Review {verb} PR #{number} in {repo}")),
    }
}

fn decode_pr_review_comment(action: Option<&str>, body: &Value) -> Option<String> {
    let number = body
        .pointer("/pull_request/number")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let repo = repo_full_name(body);
    let author = body.pointer("/comment/user/login").and_then(Value::as_str);
    let verb = match action {
        Some("created") => "left an inline review comment on",
        Some("edited") => "edited an inline review comment on",
        Some("deleted") => "deleted an inline review comment on",
        Some(o) => o,
        None => "commented on",
    };

    match author {
        Some(u) => Some(format!("@{u} {verb} PR #{number} in {repo}")),
        None => Some(format!("{verb} PR #{number} in {repo}")),
    }
}

fn decode_issue_comment(action: Option<&str>, body: &Value) -> Option<String> {
    // `issue_comment` fires for BOTH issue comments and PR conversation
    // comments. PRs have a `pull_request` field inside the `issue` object.
    let repo = repo_full_name(body);
    let number = body
        .pointer("/issue/number")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let is_pr = body.pointer("/issue/pull_request").is_some();
    let author = body.pointer("/comment/user/login").and_then(Value::as_str);
    let comment_body = body
        .pointer("/comment/body")
        .and_then(Value::as_str)
        .unwrap_or("");

    // Detect CodeRabbit review comments specifically — they batch-post multiple
    // review comments using the issue_comment endpoint.
    let coderabbit =
        author.is_some_and(|u| u.eq_ignore_ascii_case("coderabbitai") || u.contains("coderabbit"));

    let target = if is_pr {
        format!("PR #{number}")
    } else {
        format!("issue #{number}")
    };

    let excerpt = if comment_body.is_empty() {
        String::new()
    } else {
        format!(": \"{}\"", truncate_inline(comment_body, 100))
    };

    let verb = match action {
        Some("created") if coderabbit => "left review comments on",
        Some("created") => "commented on",
        Some("edited") => "edited a comment on",
        Some("deleted") => "deleted a comment on",
        Some(o) => o,
        None => "commented on",
    };

    match author {
        Some(u) => Some(format!("@{u} {verb} {target} in {repo}{excerpt}")),
        None => Some(format!("{verb} {target} in {repo}{excerpt}")),
    }
}

fn decode_issues(action: Option<&str>, body: &Value) -> Option<String> {
    let repo = repo_full_name(body);
    let number = body
        .pointer("/issue/number")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let title = body
        .pointer("/issue/title")
        .and_then(Value::as_str)
        .unwrap_or("(untitled)");
    let sender = sender_login(body);

    let line = match action {
        Some("opened") => format!(
            "Issue #{number} opened in {repo}: {}",
            truncate_inline(title, 70)
        ),
        Some("closed") => format!("Issue #{number} closed in {repo}"),
        Some("reopened") => format!("Issue #{number} reopened in {repo}"),
        Some("labeled") => {
            let label = body
                .pointer("/label/name")
                .and_then(Value::as_str)
                .unwrap_or("<label>");
            format!("Issue #{number} labeled \"{label}\" in {repo}")
        }
        Some(o) => format!("Issue #{number} {o} in {repo}"),
        None => format!("Issue #{number} event in {repo}"),
    };
    Some(match sender {
        Some(u) => format!("{line} (by @{u})"),
        None => line,
    })
}

fn decode_check_run(body: &Value) -> Option<String> {
    let cr = body.get("check_run")?;
    let name = cr.get("name").and_then(Value::as_str).unwrap_or("check");
    let status = cr
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let conclusion = cr.get("conclusion").and_then(Value::as_str);
    let run_id = cr
        .get("id")
        .and_then(Value::as_u64)
        .map(|n| n.to_string())
        .or_else(|| {
            cr.get("external_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
    let repo = repo_full_name(body);

    // PR context: check_runs carry pull_requests[] in the payload.
    let pr_number = cr
        .pointer("/pull_requests/0/number")
        .and_then(Value::as_u64);

    let state = match (status, conclusion) {
        (_, Some(c)) => c,
        ("queued", _) => "queued",
        ("in_progress", _) => "running",
        ("completed", _) => "completed",
        (s, _) => s,
    };

    let pr_suffix = match pr_number {
        Some(n) => format!(" on PR #{n}"),
        None => String::new(),
    };
    let run_suffix = match run_id {
        Some(id) => format!(" (run {id})"),
        None => String::new(),
    };

    Some(format!(
        "CI check \"{name}\"{pr_suffix} in {repo} → {state}{run_suffix}"
    ))
}

fn decode_check_suite(body: &Value) -> Option<String> {
    let cs = body.get("check_suite")?;
    let status = cs
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let conclusion = cs.get("conclusion").and_then(Value::as_str);
    let repo = repo_full_name(body);
    let sha = cs.get("head_sha").and_then(Value::as_str).map(short_sha);

    let state = conclusion.unwrap_or(status);
    let sha_suffix = sha.map(|s| format!(" ({s})")).unwrap_or_default();

    Some(format!("Check suite in {repo} → {state}{sha_suffix}"))
}

fn decode_push(body: &Value) -> Option<String> {
    let ref_name = body.get("ref").and_then(Value::as_str).unwrap_or("<ref>");
    let branch = ref_name.strip_prefix("refs/heads/").unwrap_or(ref_name);
    let repo = repo_full_name(body);
    let count = body
        .get("commits")
        .and_then(Value::as_array)
        .map_or(0, std::vec::Vec::len);
    let pusher = body
        .pointer("/pusher/name")
        .and_then(Value::as_str)
        .or_else(|| body.pointer("/sender/login").and_then(Value::as_str))
        .map(str::to_string);
    let head = body.get("after").and_then(Value::as_str).map(short_sha);

    let head_suffix = head.map(|s| format!(" (head {s})")).unwrap_or_default();
    let pusher_suffix = pusher.map(|u| format!(" by @{u}")).unwrap_or_default();

    Some(format!(
        "Push: {count} commit(s) to {branch} in {repo}{head_suffix}{pusher_suffix}"
    ))
}

fn decode_workflow_run(body: &Value) -> Option<String> {
    let wr = body.get("workflow_run")?;
    let name = wr.get("name").and_then(Value::as_str).unwrap_or("workflow");
    let status = wr
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let conclusion = wr.get("conclusion").and_then(Value::as_str);
    let repo = repo_full_name(body);
    let run_number = wr.get("run_number").and_then(Value::as_u64);

    let state = conclusion.unwrap_or(status);
    let run_suffix = run_number
        .map(|n| format!(" (run #{n})"))
        .unwrap_or_default();

    Some(format!(
        "Workflow \"{name}\"{run_suffix} in {repo} → {state}"
    ))
}

fn short_sha(s: &str) -> String {
    s.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::super::decode;
    use serde_json::json;

    #[test]
    fn pull_request_merged_matches_spec_format() {
        let body = json!({
            "action": "closed",
            "pull_request": {
                "number": 658,
                "title": "fix: something",
                "merged": true,
                "merge_commit_sha": "e22f87a1234567890",
                "base": { "ref": "main" }
            },
            "repository": { "full_name": "fireflypro/firefly-pro-crm" },
            "sender": { "login": "garysomerhalder" }
        });
        let d = decode("github", Some("pull_request"), &body);
        assert_eq!(
            d.summary,
            "[GITHUB] PR #658 merged to main by @garysomerhalder (sha e22f87a) in fireflypro/firefly-pro-crm"
        );
    }

    #[test]
    fn pull_request_closed_without_merge() {
        let body = json!({
            "action": "closed",
            "pull_request": {
                "number": 100,
                "title": "wip",
                "merged": false
            },
            "repository": { "full_name": "o/r" }
        });
        let d = decode("github", Some("pull_request"), &body);
        assert_eq!(d.summary, "[GITHUB] PR #100 closed (no merge) in o/r: wip");
    }

    #[test]
    fn check_run_completion_matches_spec_format() {
        let body = json!({
            "action": "completed",
            "check_run": {
                "id": 24829199698u64,
                "name": "Client Tests",
                "status": "completed",
                "conclusion": "failure",
                "pull_requests": [{ "number": 658 }]
            },
            "repository": { "full_name": "fireflypro/firefly-pro-crm" }
        });
        let d = decode("github", Some("check_run"), &body);
        assert_eq!(
            d.summary,
            "[GITHUB] CI check \"Client Tests\" on PR #658 in fireflypro/firefly-pro-crm → failure (run 24829199698)"
        );
    }

    #[test]
    fn check_run_without_pr_context_still_decodes() {
        let body = json!({
            "action": "completed",
            "check_run": {
                "id": 1,
                "name": "build",
                "status": "completed",
                "conclusion": "success"
            },
            "repository": { "full_name": "o/r" }
        });
        let d = decode("github", Some("check_run"), &body);
        assert_eq!(
            d.summary,
            "[GITHUB] CI check \"build\" in o/r → success (run 1)"
        );
    }

    #[test]
    fn push_counts_commits_and_names_branch() {
        let body = json!({
            "ref": "refs/heads/main",
            "after": "5a0201f0123456789abcdef",
            "commits": [{ "id": "1" }, { "id": "2" }, { "id": "3" }],
            "repository": { "full_name": "o/r" },
            "pusher": { "name": "gary" }
        });
        let d = decode("github", Some("push"), &body);
        assert_eq!(
            d.summary,
            "[GITHUB] Push: 3 commit(s) to main in o/r (head 5a0201f) by @gary"
        );
    }

    #[test]
    fn pr_review_approved_names_reviewer() {
        let body = json!({
            "action": "submitted",
            "pull_request": { "number": 42 },
            "review": { "state": "approved", "user": { "login": "pedro" } },
            "repository": { "full_name": "o/r" }
        });
        let d = decode("github", Some("pull_request_review"), &body);
        assert_eq!(d.summary, "[GITHUB] @pedro approved PR #42 in o/r");
    }

    #[test]
    fn issue_comment_on_pr_includes_excerpt() {
        let body = json!({
            "action": "created",
            "issue": {
                "number": 658,
                "pull_request": { "url": "..." }
            },
            "comment": {
                "user": { "login": "reviewer" },
                "body": "looks good to me"
            },
            "repository": { "full_name": "o/r" }
        });
        let d = decode("github", Some("issue_comment"), &body);
        assert_eq!(
            d.summary,
            "[GITHUB] @reviewer commented on PR #658 in o/r: \"looks good to me\""
        );
    }

    #[test]
    fn coderabbit_comment_uses_review_verb() {
        let body = json!({
            "action": "created",
            "issue": {
                "number": 658,
                "pull_request": {}
            },
            "comment": {
                "user": { "login": "coderabbitai[bot]" },
                "body": "3 nits and a blocker"
            },
            "repository": { "full_name": "o/r" }
        });
        let d = decode("github", Some("issue_comment"), &body);
        assert!(d.summary.contains("left review comments"));
        assert!(d.summary.contains("PR #658"));
    }

    #[test]
    fn workflow_run_completed_surfaces_conclusion() {
        let body = json!({
            "action": "completed",
            "workflow_run": {
                "name": "CI",
                "status": "completed",
                "conclusion": "success",
                "run_number": 123
            },
            "repository": { "full_name": "o/r" }
        });
        let d = decode("github", Some("workflow_run"), &body);
        assert_eq!(
            d.summary,
            "[GITHUB] Workflow \"CI\" (run #123) in o/r → success"
        );
    }

    #[test]
    fn unknown_github_event_falls_back() {
        let body = json!({
            "action": "foo",
            "repository": { "full_name": "o/r" },
            "id": "delivery123"
        });
        let d = decode("github", Some("ping"), &body);
        // No decoder for ping → generic fallback
        assert!(d.summary.starts_with("[HOOKDECK:github] ping"));
    }

    #[test]
    fn missing_event_type_infers_from_body() {
        let body = json!({
            "action": "closed",
            "pull_request": {
                "number": 1,
                "title": "x",
                "merged": true,
                "merge_commit_sha": "abc1234",
                "base": { "ref": "main" }
            },
            "repository": { "full_name": "o/r" }
        });
        let d = decode("github", None, &body);
        assert!(d.summary.contains("PR #1 merged"));
    }
}
