//! Tier B remediation layer — the enforcer as an ASSISTANT, not a bouncer.
//!
//! When a started ticket is caught un-ready, the bouncer model reverts it and
//! tells the human to fix it. The assistant model instead **fixes it in place**:
//! it fills every dev-ready field it can confidently infer (Type/Area labels,
//! estimate, priority, acceptance criteria) from the ticket's title/description
//! using the codified ticket-quality rubric, so the ticket becomes dev-ready and
//! STAYS in progress. Only gaps it genuinely can't infer are escalated (Q&A in
//! an interactive session, or a tagged comment in the headless daemon), and a
//! revert is the last resort — only when nothing could be remediated.
//!
//! This is a natural `langgraph-core` `StateGraph`:
//! ```text
//!   classify -> remediate -> verify --(now ready)----------> clear
//!                              |
//!                              +--(gaps remain)--> escalate --(unresolved)--> revert
//! ```
//! The graph holds the decision + a checkpointed audit trail (what was proposed,
//! applied, escalated, why). The I/O — the `Codex` proposal and the Linear
//! mutations — runs in the async orchestrator around the sync nodes.

use std::sync::Arc;

use langgraph_core::application::services::GraphCompiler;
use langgraph_core::domain::value_objects::{NodeError, END, START};
use langgraph_core::{SqliteCheckpointer, StateGraphBuilder};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use sentinel_application::delegation_service::{delegate, DelegationRequest, Worker};
use sentinel_domain::ports::LlmPort;

const LINEAR_GRAPHQL_URL: &str = "https://api.linear.app/graphql";

/// The team's available labels, name->id, so the model picks a NAME from a
/// closed set and we map it to a real Linear `labelId` (the model can't invent
/// ids). Built once per team from the Linear label catalog.
#[derive(Debug, Clone, Default)]
pub struct LabelCatalog {
    /// Type-group labels (e.g. Bug/Feature/Enhancement) -> id.
    pub type_labels: Vec<(String, String)>,
    /// Area-group labels -> id.
    pub area_labels: Vec<(String, String)>,
}

impl LabelCatalog {
    /// Resolve a label name (case-insensitive) to its id within a group.
    #[must_use]
    fn resolve(group: &[(String, String)], name: &str) -> Option<String> {
        group
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name.trim()))
            .map(|(_, id)| id.clone())
    }

    /// Comma-list of Type label names (for the proposal prompt's closed set).
    #[must_use]
    fn type_names(&self) -> String {
        self.type_labels.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")
    }

    /// Comma-list of Area label names.
    #[must_use]
    fn area_names(&self) -> String {
        self.area_labels.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")
    }
}

/// A proposed set of field values to make a ticket dev-ready, plus the gaps the
/// model could NOT confidently fill (which get escalated, not guessed).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemediationPlan {
    /// Chosen Type label name (must be one of the catalog's), if proposed.
    pub type_label: Option<String>,
    /// Chosen Area label name, if proposed.
    pub area_label: Option<String>,
    /// Proposed fibonacci estimate, if confidently inferable.
    pub estimate: Option<i64>,
    /// Proposed priority 1..=4, if inferable (often left to a human).
    pub priority: Option<i64>,
    /// Drafted acceptance criteria (markdown checklist), if inferable.
    pub acceptance_criteria: Option<String>,
    /// Fields the model declined to fill — escalate these, don't guess.
    pub unfillable: Vec<String>,
    /// One-line reasoning for the audit trail.
    pub rationale: String,
}

/// Build the `Codex` proposal prompt: give it the ticket, the missing fields, the
/// codified rubric, and the CLOSED SET of label names it may pick from. Demand
/// strict JSON back, and instruct it to leave a field `null` + add it to
/// `unfillable` whenever it isn't confident — we escalate those, never guess.
#[must_use]
pub fn propose_prompt(
    identifier: &str,
    title: &str,
    description: &str,
    missing: &[String],
    catalog: &LabelCatalog,
) -> String {
    format!(
        "You are remediating a Linear ticket so it meets the Definition of Ready, fixing it in \
         place rather than bouncing it back. Fill ONLY the fields you can infer with HIGH \
         confidence from the ticket itself; for anything requiring human/planning judgment you \
         can't infer, leave it null and list it in \"unfillable\" — we will ask a human, never guess.\n\n\
         Missing dev-ready fields: [{missing}]\n\
         Allowed Type labels (pick exactly one NAME or null): [{types}]\n\
         Allowed Area labels (pick exactly one NAME or null): [{areas}]\n\
         Estimate scale: fibonacci 1,2,3,5,8 (size, not priority). Priority: 1=urgent..4=low.\n\
         Acceptance criteria: a markdown checklist of >=3 concrete, testable items derived from intent.\n\n\
         Ticket {identifier}: {title}\n\
         Description:\n{description}\n\n\
         Reply with STRICT JSON only, no prose, this exact shape:\n\
         {{\"type_label\":<name|null>,\"area_label\":<name|null>,\"estimate\":<int|null>,\
         \"priority\":<int|null>,\"acceptance_criteria\":<markdown string|null>,\
         \"unfillable\":[<field names you left null on purpose>],\"rationale\":\"<one line>\"}}",
        missing = missing.join(", "),
        types = catalog.type_names(),
        areas = catalog.area_names(),
    )
}

/// Parse the model's JSON reply into a [`RemediationPlan`]. Tolerant of code
/// fences / surrounding prose: extracts the first `{..}` block. Any parse
/// failure yields an all-`None` plan with everything marked unfillable, so a
/// malformed proposal escalates rather than silently mis-fills.
#[must_use]
pub fn parse_plan(reply: &str, missing: &[String]) -> RemediationPlan {
    let Some(v) = extract_json(reply) else {
        return RemediationPlan {
            unfillable: missing.to_vec(),
            rationale: "proposal unparseable — escalating all gaps".into(),
            ..Default::default()
        };
    };
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let i = |k: &str| v.get(k).and_then(Value::as_i64);
    RemediationPlan {
        type_label: s("type_label"),
        area_label: s("area_label"),
        estimate: i("estimate"),
        priority: i("priority").filter(|p| (1..=4).contains(p)),
        acceptance_criteria: s("acceptance_criteria"),
        unfillable: v
            .get("unfillable")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default(),
        rationale: s("rationale").unwrap_or_default(),
    }
}

/// Extract the first JSON object from a possibly-fenced reply.
///
/// Uses serde's streaming deserializer (which correctly ignores braces that
/// appear INSIDE string values — e.g. `${VAR}` or `{ }` in an acceptance-
/// criteria string) rather than a naive brace counter, which mis-terminated
/// the object early and silently dropped fields like `type_label`.
fn extract_json(reply: &str) -> Option<Value> {
    let start = reply.find('{')?;
    let mut stream = serde_json::Deserializer::from_str(&reply[start..]).into_iter::<Value>();
    match stream.next() {
        Some(Ok(v @ Value::Object(_))) => Some(v),
        _ => None,
    }
}

/// Fetch a ticket's team label catalog (Type + Area groups) so the remediator
/// maps a chosen label NAME to a real `labelId`. Returns an empty catalog on
/// failure (remediation then can't fill labels → escalates them, never guesses).
pub async fn fetch_label_catalog(
    client: &reqwest::Client,
    token: &str,
    issue_id: &str,
) -> LabelCatalog {
    let query = format!(
        r#"{{"query":"query{{issue(id:\"{issue_id}\"){{team{{labels(first:100){{nodes{{id name parent{{name}}}}}}}}}}}}"}}"#
    );
    let Ok(resp) = client
        .post(LINEAR_GRAPHQL_URL)
        .header("Authorization", token)
        .header("Content-Type", "application/json")
        .body(query)
        .send()
        .await
    else {
        return LabelCatalog::default();
    };
    let Ok(body) = resp.json::<Value>().await else {
        return LabelCatalog::default();
    };
    let nodes = body
        .get("data")
        .and_then(|d| d.get("issue"))
        .and_then(|i| i.get("team"))
        .and_then(|t| t.get("labels"))
        .and_then(|l| l.get("nodes"))
        .and_then(Value::as_array);
    let mut cat = LabelCatalog::default();
    if let Some(nodes) = nodes {
        for n in nodes {
            let (Some(id), Some(name)) = (
                n.get("id").and_then(Value::as_str),
                n.get("name").and_then(Value::as_str),
            ) else {
                continue;
            };
            match n.get("parent").and_then(|p| p.get("name")).and_then(Value::as_str) {
                Some("Type") => cat.type_labels.push((name.to_string(), id.to_string())),
                Some("Area") => cat.area_labels.push((name.to_string(), id.to_string())),
                _ => {}
            }
        }
    }
    cat
}

/// Fetch a ticket's `(title, description)` for the proposal prompt. Empty
/// strings on failure (the proposal then has less to work with → more unfillable).
pub async fn fetch_ticket_text(
    client: &reqwest::Client,
    token: &str,
    issue_id: &str,
) -> (String, String) {
    let query = format!(
        r#"{{"query":"query{{issue(id:\"{issue_id}\"){{title description}}}}"}}"#
    );
    let val: Option<Value> = async {
        client
            .post(LINEAR_GRAPHQL_URL)
            .header("Authorization", token)
            .header("Content-Type", "application/json")
            .body(query)
            .send()
            .await
            .ok()?
            .json::<Value>()
            .await
            .ok()
    }
    .await;
    let issue = val.as_ref().and_then(|v| v.get("data")).and_then(|d| d.get("issue"));
    let title = issue
        .and_then(|i| i.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let desc = issue
        .and_then(|i| i.get("description"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    (title, desc)
}

/// Post a comment on a ticket (the audit trail / escalation channel). Best-effort
/// and IDEMPOTENT: if a comment with the same body already exists in the ticket's
/// recent history, it is skipped. Without this the enforcer re-posts the same
/// dev-ready / discipline / SLA comment on every repeat `IssueChanged` event AND
/// on every daemon restart (the in-memory debounce resets), which spammed tickets
/// with dozens of identical copies. The dedup is body-exact, so genuinely new
/// content (e.g. a different missing-field set) still posts.
pub async fn post_comment(client: &reqwest::Client, token: &str, issue_id: &str, body: &str) {
    if comment_exists(client, token, issue_id, body).await {
        return;
    }
    let mutation = serde_json::json!({
        "query": "mutation($id:String!,$b:String!){ commentCreate(input:{issueId:$id,body:$b}){ success } }",
        "variables": { "id": issue_id, "b": body }
    })
    .to_string();
    let _ = post_ok(client, token, mutation, "commentCreate").await;
}

/// Does an identical comment body already exist in the ticket's recent comments?
/// Best-effort: on any fetch error returns `false` (so a transient read failure
/// doesn't suppress a legitimately-needed comment). Checks the most recent 50.
async fn comment_exists(
    client: &reqwest::Client,
    token: &str,
    issue_id: &str,
    body: &str,
) -> bool {
    let query = format!(
        r#"{{"query":"query{{issue(id:\"{issue_id}\"){{comments(first:50){{nodes{{body}}}}}}}}"}}"#
    );
    let val: Option<Value> = async {
        client
            .post(LINEAR_GRAPHQL_URL)
            .header("Authorization", token)
            .header("Content-Type", "application/json")
            .body(query)
            .send()
            .await
            .ok()?
            .json::<Value>()
            .await
            .ok()
    }
    .await;
    val.as_ref()
        .and_then(|v| v.get("data"))
        .and_then(|d| d.get("issue"))
        .and_then(|i| i.get("comments"))
        .and_then(|c| c.get("nodes"))
        .and_then(Value::as_array)
        .is_some_and(|nodes| {
            nodes
                .iter()
                .filter_map(|n| n.get("body").and_then(Value::as_str))
                .any(|existing| existing == body)
        })
}

/// Run the `Codex` proposal for a ticket. Errors (or an unreachable model) yield
/// an all-unfillable plan so the flow escalates rather than mis-fills.
pub async fn propose_remediation(
    llm: &dyn LlmPort,
    identifier: &str,
    title: &str,
    description: &str,
    missing: &[String],
    catalog: &LabelCatalog,
) -> RemediationPlan {
    let req = DelegationRequest {
        worker: Worker::Codex,
        // gpt-5.5-pro is a reasoning model: max_tokens caps reasoning + output
        // combined. Give generous headroom so the JSON proposal lands after the
        // model reasons (a tight cap is fully consumed by reasoning → empty).
        task: propose_prompt(identifier, title, description, missing, catalog),
        context: String::new(),
        max_tokens: 3000,
    };
    tracing::debug!(ticket = %identifier, "linear enforcer: requesting remediation proposal (Codex)");
    match delegate(llm, &req).await {
        Ok(res) => {
            tracing::debug!(ticket = %identifier, raw = %res.output.chars().take(300).collect::<String>(), "linear enforcer: remediation proposal received");
            parse_plan(&res.output, missing)
        }
        Err(e) => {
            tracing::warn!(ticket = %identifier, error = %e, "linear enforcer: remediation proposal failed — escalating");
            RemediationPlan {
                unfillable: missing.to_vec(),
                rationale: format!("proposal failed ({e}) — escalating all gaps"),
                ..Default::default()
            }
        }
    }
}

/// Apply a remediation plan to a ticket via the Linear REST GraphQL. Returns the
/// list of human-readable changes actually applied (for the audit comment).
/// Best-effort per field: a failed mutation is logged and skipped, not fatal.
pub async fn apply_remediation(
    client: &reqwest::Client,
    token: &str,
    issue_id: &str,
    plan: &RemediationPlan,
    catalog: &LabelCatalog,
) -> Vec<String> {
    let mut applied = Vec::new();
    tracing::debug!(
        issue = %issue_id,
        type_label = ?plan.type_label, area_label = ?plan.area_label,
        catalog_types = catalog.type_labels.len(), catalog_areas = catalog.area_labels.len(),
        "linear enforcer: applying remediation plan"
    );

    // Labels: map the chosen NAME -> real labelId, then issueAddLabel.
    if let Some(name) = &plan.type_label {
        if let Some(id) = LabelCatalog::resolve(&catalog.type_labels, name) {
            let ok = add_label(client, token, issue_id, &id).await;
            tracing::debug!(label = %name, %id, ok, "linear enforcer: add Type label");
            if ok {
                applied.push(format!("Type label -> {name}"));
            }
        } else {
            tracing::warn!(label = %name, "linear enforcer: Type label not in catalog — cannot apply");
        }
    }
    if let Some(name) = &plan.area_label {
        if let Some(id) = LabelCatalog::resolve(&catalog.area_labels, name) {
            if add_label(client, token, issue_id, &id).await {
                applied.push(format!("Area label -> {name}"));
            }
        } else {
            tracing::warn!(label = %name, "linear enforcer: Area label not in catalog — cannot apply");
        }
    }

    // Scalar fields via issueUpdate.
    if let Some(est) = plan.estimate {
        if issue_update(client, token, issue_id, &format!("estimate:{est}")).await {
            applied.push(format!("estimate -> {est}"));
        }
    }
    if let Some(pri) = plan.priority {
        if issue_update(client, token, issue_id, &format!("priority:{pri}")).await {
            applied.push(format!("priority -> {pri}"));
        }
    }

    if !applied.is_empty() {
        tracing::info!(issue = %issue_id, changes = %applied.join("; "), "linear enforcer: auto-remediated ticket in place");
    }
    applied
}

async fn add_label(client: &reqwest::Client, token: &str, issue_id: &str, label_id: &str) -> bool {
    let body = serde_json::json!({
        "query": "mutation($id:String!,$l:String!){ issueAddLabel(id:$id, labelId:$l){ success } }",
        "variables": { "id": issue_id, "l": label_id }
    })
    .to_string();
    post_ok(client, token, body, "issueAddLabel").await
}

async fn issue_update(client: &reqwest::Client, token: &str, issue_id: &str, input_frag: &str) -> bool {
    let mutation = format!(
        r#"{{"query":"mutation{{issueUpdate(id:\"{issue_id}\",input:{{{input_frag}}}){{success}}}}"}}"#
    );
    post_ok(client, token, mutation, "issueUpdate").await
}

async fn post_ok(client: &reqwest::Client, token: &str, body: String, op: &str) -> bool {
    match client
        .post(LINEAR_GRAPHQL_URL)
        .header("Authorization", token)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
    {
        Ok(r) => match r.json::<Value>().await {
            Ok(v) => {
                let ok = v
                    .get("data")
                    .and_then(|d| d.get(op))
                    .and_then(|o| o.get("success"))
                    .and_then(Value::as_bool)
                    == Some(true);
                if !ok {
                    tracing::warn!(%op, resp = %v, "linear enforcer: remediation mutation not ok");
                }
                ok
            }
            Err(e) => {
                tracing::warn!(%op, error = %e, "linear enforcer: remediation response parse failed");
                false
            }
        },
        Err(e) => {
            tracing::warn!(%op, error = %e, "linear enforcer: remediation request failed");
            false
        }
    }
}

// ----- the remediation decision graph -----------------------------------

/// Terminal outcome of the remediation flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RemediationOutcome {
    /// Ticket is now dev-ready (was already, or remediation filled the gaps) —
    /// it stays in progress. The happy path.
    #[default]
    Clear,
    /// Some gaps remain that need human judgment — escalate (Q&A / comment),
    /// ticket stays in progress pending the human.
    Escalate,
    /// Nothing could be remediated and it's egregious — revert (last resort).
    Revert,
}

/// Graph state: the readiness facts + what remediation achieved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationState {
    pub identifier: String,
    /// Missing fields BEFORE remediation.
    pub missing_before: Vec<String>,
    /// Count of fields successfully filled in place.
    pub applied_count: usize,
    /// Fields still missing AFTER remediation (escalation set).
    pub still_missing: Vec<String>,
    pub outcome: RemediationOutcome,
}

const CLASSIFY: &str = "classify";
const VERIFY: &str = "verify";
const CLEAR: &str = "clear";
const ESCALATE: &str = "escalate";
const REVERT: &str = "revert";

/// Build the remediation decision graph. Pure; the I/O (propose + apply +
/// re-fetch) runs in the orchestrator, which seeds `applied_count` /
/// `still_missing` before invoking. `verify` routes on the post-remediation
/// state: nothing-still-missing -> clear; some-filled-but-gaps -> escalate;
/// nothing-filled-and-gaps -> revert (last resort).
///
/// # Errors
/// Returns the stringified `langgraph-core` build/compile error on failure.
pub async fn build_remediation_graph()
-> Result<langgraph_core::application::services::CompilationResult<RemediationState>, String> {
    let builder = StateGraphBuilder::<RemediationState>::new()
        .add_node(CLASSIFY, |s: &RemediationState| Ok::<_, NodeError>(s.clone()))
        .add_node(VERIFY, |s: &RemediationState| Ok::<_, NodeError>(s.clone()))
        .add_node(CLEAR, |s: &RemediationState| {
            let mut n = s.clone();
            n.outcome = RemediationOutcome::Clear;
            Ok::<_, NodeError>(n)
        })
        .add_node(ESCALATE, |s: &RemediationState| {
            let mut n = s.clone();
            n.outcome = RemediationOutcome::Escalate;
            Ok::<_, NodeError>(n)
        })
        .add_node(REVERT, |s: &RemediationState| {
            let mut n = s.clone();
            n.outcome = RemediationOutcome::Revert;
            Ok::<_, NodeError>(n)
        })
        .add_edge(START, CLASSIFY)
        .add_edge(CLASSIFY, VERIFY)
        .add_conditional_edge(VERIFY, |s: &RemediationState| {
            if s.still_missing.is_empty() {
                CLEAR.into() // fully dev-ready now
            } else if s.applied_count > 0 {
                ESCALATE.into() // partial progress -> ask the human for the rest
            } else {
                REVERT.into() // nothing fillable -> last resort
            }
        })
        .add_edge(CLEAR, END)
        .add_edge(ESCALATE, END)
        .add_edge(REVERT, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    let checkpointer = SqliteCheckpointer::new(":memory:").await.map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(Arc::new(checkpointer))
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

/// Run the remediation graph over a seeded state and return the outcome.
///
/// # Errors
/// Returns the stringified graph execution error on failure.
pub async fn run_remediation_decision(
    compiled: &langgraph_core::application::services::CompilationResult<RemediationState>,
    state: RemediationState,
) -> Result<RemediationOutcome, String> {
    use langgraph_core::prelude::ExecutableGraph;
    let out = compiled.graph.invoke(state).await.map_err(|e| e.to_string())?;
    Ok(out.outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> LabelCatalog {
        LabelCatalog {
            type_labels: vec![
                ("Bug".into(), "bug-id".into()),
                ("Feature".into(), "feat-id".into()),
                ("Enhancement".into(), "enh-id".into()),
            ],
            area_labels: vec![("Settings".into(), "settings-id".into())],
        }
    }

    #[test]
    fn resolve_label_case_insensitive() {
        let c = catalog();
        assert_eq!(LabelCatalog::resolve(&c.type_labels, "bug"), Some("bug-id".into()));
        assert_eq!(LabelCatalog::resolve(&c.type_labels, " Feature "), Some("feat-id".into()));
        assert_eq!(LabelCatalog::resolve(&c.type_labels, "Nope"), None);
    }

    #[test]
    fn prompt_includes_closed_label_set_and_unfillable_instruction() {
        let p = propose_prompt(
            "FPCRM-596",
            "Isolate the CI test database from staging",
            "test suite writes to staging",
            &["Type label".into()],
            &catalog(),
        );
        assert!(p.contains("FPCRM-596"));
        assert!(p.contains("Bug, Feature, Enhancement"), "closed Type set must be listed");
        assert!(p.contains("unfillable"), "must instruct to escalate, not guess");
        assert!(p.contains("STRICT JSON"));
    }

    #[test]
    fn parse_plan_extracts_fenced_json() {
        let reply = "Here you go:\n```json\n{\"type_label\":\"Bug\",\"area_label\":null,\
                     \"estimate\":3,\"priority\":null,\"acceptance_criteria\":\"- [ ] a\\n- [ ] b\\n- [ ] c\",\
                     \"unfillable\":[\"priority\"],\"rationale\":\"infra correctness bug\"}\n```";
        let plan = parse_plan(reply, &["Type label".into()]);
        assert_eq!(plan.type_label.as_deref(), Some("Bug"));
        assert_eq!(plan.estimate, Some(3));
        assert_eq!(plan.priority, None);
        assert_eq!(plan.unfillable, vec!["priority".to_string()]);
        assert!(plan.acceptance_criteria.unwrap().contains("- [ ]"));
    }

    #[test]
    fn parse_plan_clamps_bad_priority() {
        let plan = parse_plan("{\"priority\":9,\"unfillable\":[]}", &[]);
        assert_eq!(plan.priority, None, "out-of-range priority dropped, not applied");
    }

    #[test]
    fn parse_plan_handles_braces_inside_string_values() {
        // Regression: a naive brace-counter terminated the object early when a
        // STRING value contained `{`/`}` (e.g. a `${VAR}` snippet in the
        // acceptance criteria), silently dropping `type_label`. The serde
        // streaming extractor must read the whole object regardless.
        let reply = "{\"type_label\":\"Bug\",\"area_label\":null,\
                     \"acceptance_criteria\":\"- [ ] set env ${TEST_DATABASE_URL} = {isolated}\",\
                     \"unfillable\":[],\"rationale\":\"x\"}";
        let plan = parse_plan(reply, &["Type label".into()]);
        assert_eq!(plan.type_label.as_deref(), Some("Bug"), "type_label must survive braces in a later string");
        assert!(plan.acceptance_criteria.unwrap().contains("${TEST_DATABASE_URL}"));
    }

    #[test]
    fn parse_plan_unparseable_escalates_everything() {
        let plan = parse_plan("the model rambled with no json", &["Type label".into(), "estimate".into()]);
        assert!(plan.type_label.is_none());
        assert_eq!(plan.unfillable, vec!["Type label".to_string(), "estimate".to_string()]);
    }

    #[tokio::test]
    async fn graph_clears_when_fully_remediated() {
        let g = build_remediation_graph().await.unwrap();
        let s = RemediationState {
            identifier: "FPCRM-596".into(),
            missing_before: vec!["Type label".into()],
            applied_count: 1,
            still_missing: vec![],
            outcome: RemediationOutcome::Clear,
        };
        assert_eq!(run_remediation_decision(&g, s).await.unwrap(), RemediationOutcome::Clear);
    }

    #[tokio::test]
    async fn graph_escalates_on_partial_remediation() {
        let g = build_remediation_graph().await.unwrap();
        let s = RemediationState {
            identifier: "X-1".into(),
            missing_before: vec!["Type label".into(), "acceptance criteria".into()],
            applied_count: 1,                                  // filled the label
            still_missing: vec!["acceptance criteria".into()], // but AC needs a human
            outcome: RemediationOutcome::Clear,
        };
        assert_eq!(run_remediation_decision(&g, s).await.unwrap(), RemediationOutcome::Escalate);
    }

    #[tokio::test]
    async fn graph_reverts_when_nothing_fillable() {
        let g = build_remediation_graph().await.unwrap();
        let s = RemediationState {
            identifier: "X-2".into(),
            missing_before: vec!["everything".into()],
            applied_count: 0, // couldn't fill anything
            still_missing: vec!["everything".into()],
            outcome: RemediationOutcome::Clear,
        };
        assert_eq!(run_remediation_decision(&g, s).await.unwrap(), RemediationOutcome::Revert);
    }
}
