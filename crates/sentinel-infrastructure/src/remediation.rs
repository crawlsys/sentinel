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
//! mutations — runs in the async orchestrator; the graph nodes run on
//! LangGraph-Rust's native async execution path.

use std::time::Duration;

use langgraph_core::application::services::GraphCompiler;
use langgraph_core::domain::value_objects::{
    NodeConfig, NodeError, NodeTimeoutPolicy, StateError, StateSchema, END, START,
};
use langgraph_core::StateGraphBuilder;
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
        self.type_labels
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Comma-list of Area label names.
    #[must_use]
    fn area_names(&self) -> String {
        self.area_labels
            .iter()
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ")
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
    /// The proposal call failed or returned unusable JSON. This is not the same
    /// as a valid proposal deciding that fields are genuinely unfillable.
    #[serde(default)]
    pub proposal_failed: bool,
}

impl RemediationPlan {
    /// Count the Linear mutations this plan can actually apply with the current
    /// catalog. This is used as the graph's pre-apply authorization input, so
    /// unresolved label names do not look like real remediations.
    #[must_use]
    pub fn applicable_change_count(&self, catalog: &LabelCatalog) -> usize {
        usize::from(
            self.type_label
                .as_deref()
                .and_then(|name| LabelCatalog::resolve(&catalog.type_labels, name))
                .is_some(),
        ) + usize::from(
            self.area_label
                .as_deref()
                .and_then(|name| LabelCatalog::resolve(&catalog.area_labels, name))
                .is_some(),
        ) + usize::from(self.estimate.is_some())
            + usize::from(self.priority.is_some())
    }
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
        return failed_plan(missing, "proposal unparseable — escalating all gaps");
    };
    let type_label = match optional_string_field(&v, "type_label", missing) {
        Ok(value) => value,
        Err(plan) => return plan,
    };
    let area_label = match optional_string_field(&v, "area_label", missing) {
        Ok(value) => value,
        Err(plan) => return plan,
    };
    let estimate = match optional_i64_field(&v, "estimate", missing) {
        Ok(Some(value)) if matches!(value, 1 | 2 | 3 | 5 | 8) => Some(value),
        Ok(Some(value)) => {
            return failed_plan(
                missing,
                format!("proposal estimate {value} is outside the allowed scale"),
            );
        }
        Ok(None) => None,
        Err(plan) => return plan,
    };
    let priority = match optional_i64_field(&v, "priority", missing) {
        Ok(Some(value)) if (1..=4).contains(&value) => Some(value),
        Ok(Some(value)) => {
            return failed_plan(
                missing,
                format!("proposal priority {value} is outside the allowed range"),
            );
        }
        Ok(None) => None,
        Err(plan) => return plan,
    };
    let acceptance_criteria = match optional_string_field(&v, "acceptance_criteria", missing) {
        Ok(value) => value,
        Err(plan) => return plan,
    };
    let unfillable = match unfillable_field(&v, missing) {
        Ok(value) => value,
        Err(plan) => return plan,
    };
    let rationale = match required_string_field(&v, "rationale", missing) {
        Ok(value) => value,
        Err(plan) => return plan,
    };

    RemediationPlan {
        type_label,
        area_label,
        estimate,
        priority,
        acceptance_criteria,
        unfillable,
        rationale,
        proposal_failed: false,
    }
}

fn failed_plan(missing: &[String], rationale: impl Into<String>) -> RemediationPlan {
    RemediationPlan {
        unfillable: missing.to_vec(),
        rationale: rationale.into(),
        proposal_failed: true,
        ..Default::default()
    }
}

fn optional_string_field(
    value: &Value,
    field: &str,
    missing: &[String],
) -> Result<Option<String>, RemediationPlan> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(|text| Some(text.to_string()))
            .ok_or_else(|| {
                failed_plan(
                    missing,
                    format!("proposal field {field} must be string or null"),
                )
            }),
    }
}

fn optional_i64_field(
    value: &Value,
    field: &str,
    missing: &[String],
) -> Result<Option<i64>, RemediationPlan> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_i64().map(Some).ok_or_else(|| {
            failed_plan(
                missing,
                format!("proposal field {field} must be integer or null"),
            )
        }),
    }
}

fn required_string_field(
    value: &Value,
    field: &str,
    missing: &[String],
) -> Result<String, RemediationPlan> {
    match value.get(field).and_then(Value::as_str) {
        Some(text) if !text.trim().is_empty() => Ok(text.to_string()),
        _ => Err(failed_plan(
            missing,
            format!("proposal field {field} must be a non-empty string"),
        )),
    }
}

fn unfillable_field(value: &Value, missing: &[String]) -> Result<Vec<String>, RemediationPlan> {
    let Some(values) = value.get("unfillable").and_then(Value::as_array) else {
        return Err(failed_plan(
            missing,
            "proposal field unfillable must be an array",
        ));
    };
    let mut unfillable = Vec::with_capacity(values.len());
    for value in values {
        let Some(field) = value.as_str() else {
            return Err(failed_plan(
                missing,
                "proposal field unfillable must contain only strings",
            ));
        };
        unfillable.push(field.to_string());
    }
    Ok(unfillable)
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
/// maps a chosen label NAME to a real `labelId`.
///
/// # Errors
/// Returns an error when Linear cannot provide a verified catalog response.
pub async fn fetch_label_catalog(
    client: &reqwest::Client,
    token: &str,
    issue_id: &str,
) -> Result<LabelCatalog, String> {
    let query = format!(
        r#"{{"query":"query{{issue(id:\"{issue_id}\"){{team{{labels(first:100){{nodes{{id name parent{{name}}}}}}}}}}}}"}}"#
    );
    let resp = client
        .post(LINEAR_GRAPHQL_URL)
        .header("Authorization", token)
        .header("Content-Type", "application/json")
        .body(query)
        .send()
        .await
        .map_err(|e| format!("fetch Linear label catalog for {issue_id}: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("read Linear label catalog response for {issue_id}: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "fetch Linear label catalog for {issue_id} returned HTTP {status}: {body}"
        ));
    }
    parse_label_catalog_response(issue_id, &body)
}

fn parse_label_catalog_response(issue_id: &str, body: &str) -> Result<LabelCatalog, String> {
    let body: Value = serde_json::from_str(body)
        .map_err(|e| format!("parse Linear label catalog response for {issue_id}: {e}"))?;
    if let Some(errors) = body.get("errors").and_then(Value::as_array) {
        if !errors.is_empty() {
            return Err(format!(
                "fetch Linear label catalog for {issue_id} returned GraphQL errors: {errors:?}"
            ));
        }
    }
    let nodes = body
        .get("data")
        .and_then(|d| d.get("issue"))
        .and_then(|i| i.get("team"))
        .and_then(|t| t.get("labels"))
        .and_then(|l| l.get("nodes"))
        .and_then(Value::as_array)
        .ok_or_else(|| format!("fetch Linear label catalog for {issue_id} omitted labels.nodes"))?;
    let mut cat = LabelCatalog::default();
    for n in nodes {
        let id = n.get("id").and_then(Value::as_str).ok_or_else(|| {
            format!("Linear label catalog for {issue_id} contains label without id")
        })?;
        let name = n.get("name").and_then(Value::as_str).ok_or_else(|| {
            format!("Linear label catalog for {issue_id} contains label without name")
        })?;
        match n
            .get("parent")
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
        {
            Some("Type") => cat.type_labels.push((name.to_string(), id.to_string())),
            Some("Area") => cat.area_labels.push((name.to_string(), id.to_string())),
            _ => {}
        }
    }
    Ok(cat)
}

/// Fetch a ticket's `(title, description)` for the proposal prompt.
///
/// # Errors
/// Returns an error when Linear cannot provide a verified ticket text response.
pub async fn fetch_ticket_text(
    client: &reqwest::Client,
    token: &str,
    issue_id: &str,
) -> Result<(String, String), String> {
    let query = format!(r#"{{"query":"query{{issue(id:\"{issue_id}\"){{title description}}}}"}}"#);
    let response = client
        .post(LINEAR_GRAPHQL_URL)
        .header("Authorization", token)
        .header("Content-Type", "application/json")
        .body(query)
        .send()
        .await
        .map_err(|e| format!("fetch Linear ticket text for {issue_id}: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("read Linear ticket text response for {issue_id}: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "fetch Linear ticket text for {issue_id} returned HTTP {status}: {body}"
        ));
    }
    parse_ticket_text_response(issue_id, &body)
}

fn parse_ticket_text_response(issue_id: &str, body: &str) -> Result<(String, String), String> {
    let val: Value = serde_json::from_str(body)
        .map_err(|e| format!("parse Linear ticket text response for {issue_id}: {e}"))?;
    if let Some(errors) = val.get("errors").and_then(Value::as_array) {
        if !errors.is_empty() {
            return Err(format!(
                "fetch Linear ticket text for {issue_id} returned GraphQL errors: {errors:?}"
            ));
        }
    }
    let issue = val.get("data").and_then(|d| d.get("issue"));
    let title = issue
        .and_then(|i| i.get("title"))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("fetch Linear ticket text for {issue_id} omitted title"))?
        .to_string();
    let desc = issue
        .and_then(|i| i.get("description"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Ok((title, desc))
}

/// Post a comment on a ticket (the audit trail / escalation channel).
///
/// Idempotent: if a comment with the same body already exists in the ticket's
/// recent history, it is skipped. Without this the enforcer re-posts the same
/// dev-ready / discipline / SLA comment on every repeat `IssueChanged` event AND
/// on every daemon restart (the in-memory debounce resets), which spammed tickets
/// with dozens of identical copies. The dedup is body-exact, so genuinely new
/// content (e.g. a different missing-field set) still posts.
///
/// # Errors
/// Returns an error when the Linear comment mutation fails or returns a failed
/// GraphQL envelope. `Ok(false)` means a duplicate body already existed.
pub(crate) async fn post_comment(
    client: &reqwest::Client,
    token: &str,
    issue_id: &str,
    body: &str,
) -> Result<bool, String> {
    if comment_exists(client, token, issue_id, body).await? {
        return Ok(false);
    }
    let mutation = serde_json::json!({
        "query": "mutation($id:String!,$b:String!){ commentCreate(input:{issueId:$id,body:$b}){ success } }",
        "variables": { "id": issue_id, "b": body }
    })
    .to_string();
    post_ok(client, token, mutation, "commentCreate").await?;
    Ok(true)
}

/// Does an identical comment body already exist in the ticket's recent comments?
///
/// Checks the most recent 50 comments before posting. The read is part of the
/// idempotent write contract; if it cannot be verified, the caller should not
/// guess and post anyway.
async fn comment_exists(
    client: &reqwest::Client,
    token: &str,
    issue_id: &str,
    body: &str,
) -> Result<bool, String> {
    let query = format!(
        r#"{{"query":"query{{issue(id:\"{issue_id}\"){{comments(first:50){{nodes{{body}}}}}}}}"}}"#
    );
    let response = client
        .post(LINEAR_GRAPHQL_URL)
        .header("Authorization", token)
        .header("Content-Type", "application/json")
        .body(query)
        .send()
        .await
        .map_err(|e| format!("fetch Linear comments for {issue_id}: {e}"))?;
    let status = response.status();
    let response_body = response
        .text()
        .await
        .map_err(|e| format!("read Linear comments response for {issue_id}: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "fetch Linear comments for {issue_id} returned HTTP {status}: {response_body}"
        ));
    }
    let val: Value = serde_json::from_str(&response_body)
        .map_err(|e| format!("parse Linear comments response for {issue_id}: {e}"))?;
    if let Some(errors) = val.get("errors").and_then(Value::as_array) {
        if !errors.is_empty() {
            return Err(format!(
                "fetch Linear comments for {issue_id} returned GraphQL errors: {errors:?}"
            ));
        }
    }
    let Some(nodes) = val
        .get("data")
        .and_then(|d| d.get("issue"))
        .and_then(|i| i.get("comments"))
        .and_then(|c| c.get("nodes"))
        .and_then(Value::as_array)
    else {
        return Err(format!(
            "fetch Linear comments for {issue_id} omitted comments.nodes"
        ));
    };
    Ok(nodes
        .iter()
        .filter_map(|n| n.get("body").and_then(Value::as_str))
        .any(|existing| existing.trim() == body.trim()))
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
                proposal_failed: true,
                ..Default::default()
            }
        }
    }
}

/// Apply a remediation plan to a ticket via the Linear REST GraphQL.
///
/// Returns the list of human-readable changes actually applied for the audit
/// comment. Every write is graph-authorized; if Linear rejects any write, the
/// remediation pass fails closed instead of treating the failed write as a
/// partial success.
///
/// # Errors
/// Returns an error when an authorized Linear mutation fails or when the plan
/// references a label that is absent from the fetched catalog.
pub async fn apply_remediation(
    client: &reqwest::Client,
    token: &str,
    issue_id: &str,
    plan: &RemediationPlan,
    catalog: &LabelCatalog,
    authorization: &RemediationApplyAuthorization,
) -> Result<Vec<String>, String> {
    let mut applied = Vec::new();
    tracing::debug!(
        issue = %issue_id,
        type_label = ?plan.type_label, area_label = ?plan.area_label,
        catalog_types = catalog.type_labels.len(), catalog_areas = catalog.area_labels.len(),
        remediation_graph_thread_id = %authorization.thread_id(),
        remediation_graph_checkpoint_id = %authorization.checkpoint_id(),
        "linear enforcer: applying remediation plan"
    );

    // Labels: map the chosen NAME -> real labelId, then issueAddLabel.
    if let Some(name) = &plan.type_label {
        if let Some(id) = LabelCatalog::resolve(&catalog.type_labels, name) {
            add_label(client, token, issue_id, &id).await?;
            tracing::debug!(label = %name, %id, "linear enforcer: add Type label");
            applied.push(format!("Type label -> {name}"));
        } else {
            return Err(format!("Type label `{name}` is absent from Linear catalog"));
        }
    }
    if let Some(name) = &plan.area_label {
        if let Some(id) = LabelCatalog::resolve(&catalog.area_labels, name) {
            add_label(client, token, issue_id, &id).await?;
            applied.push(format!("Area label -> {name}"));
        } else {
            return Err(format!("Area label `{name}` is absent from Linear catalog"));
        }
    }

    // Scalar fields via issueUpdate.
    if let Some(est) = plan.estimate {
        issue_update(client, token, issue_id, &format!("estimate:{est}")).await?;
        applied.push(format!("estimate -> {est}"));
    }
    if let Some(pri) = plan.priority {
        issue_update(client, token, issue_id, &format!("priority:{pri}")).await?;
        applied.push(format!("priority -> {pri}"));
    }

    if !applied.is_empty() {
        tracing::info!(issue = %issue_id, changes = %applied.join("; "), "linear enforcer: auto-remediated ticket in place");
    }
    Ok(applied)
}

async fn add_label(
    client: &reqwest::Client,
    token: &str,
    issue_id: &str,
    label_id: &str,
) -> Result<(), String> {
    let body = serde_json::json!({
        "query": "mutation($id:String!,$l:String!){ issueAddLabel(id:$id, labelId:$l){ success } }",
        "variables": { "id": issue_id, "l": label_id }
    })
    .to_string();
    post_ok(client, token, body, "issueAddLabel").await
}

async fn issue_update(
    client: &reqwest::Client,
    token: &str,
    issue_id: &str,
    input_frag: &str,
) -> Result<(), String> {
    let mutation = format!(
        r#"{{"query":"mutation{{issueUpdate(id:\"{issue_id}\",input:{{{input_frag}}}){{success}}}}"}}"#
    );
    post_ok(client, token, mutation, "issueUpdate").await
}

async fn post_ok(
    client: &reqwest::Client,
    token: &str,
    body: String,
    op: &str,
) -> Result<(), String> {
    let response = client
        .post(LINEAR_GRAPHQL_URL)
        .header("Authorization", token)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("{op} request failed: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("read {op} response: {e}"))?;
    if !status.is_success() {
        return Err(format!("{op} returned HTTP {status}: {body}"));
    }

    validate_linear_success(op, &body)
}

fn validate_linear_success(op: &str, body: &str) -> Result<(), String> {
    let v: Value =
        serde_json::from_str(&body).map_err(|e| format!("parse {op} response: {e}: {body}"))?;
    if let Some(errors) = v.get("errors").and_then(Value::as_array) {
        if !errors.is_empty() {
            return Err(format!("{op} returned GraphQL errors: {errors:?}"));
        }
    }
    let ok = v
        .get("data")
        .and_then(|d| d.get(op))
        .and_then(|o| o.get("success"))
        .and_then(Value::as_bool)
        == Some(true);
    if !ok {
        return Err(format!("{op} omitted success=true: {v}"));
    }
    Ok(())
}

// ----- the remediation decision graph -----------------------------------

/// Terminal outcome of the remediation flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RemediationOutcome {
    /// The graph authorizes applying the proposed remediation mutations. The
    /// orchestrator must re-enter the graph afterward with post-apply evidence.
    Apply,
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

/// Which remediation checkpoint is being evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum RemediationStage {
    /// Pre-mutation authorization. A terminal `Apply` is the only state that
    /// may trigger Linear field mutations.
    PreApply,
    /// Post-mutation verification. Produces terminal Clear/Escalate/Revert.
    #[default]
    PostApply,
}

/// Graph state: the readiness facts + what remediation achieved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationState {
    pub identifier: String,
    /// Whether this checkpoint authorizes mutation or verifies mutation output.
    #[serde(default)]
    pub stage: RemediationStage,
    /// Missing fields BEFORE remediation.
    pub missing_before: Vec<String>,
    /// Count of plan fields that can be applied with the real Linear catalog.
    #[serde(default)]
    pub planned_count: usize,
    /// Count of fields successfully filled in place.
    pub applied_count: usize,
    /// Fields still missing AFTER remediation (escalation set).
    pub still_missing: Vec<String>,
    /// The proposal layer failed or returned unusable JSON. The graph escalates
    /// this instead of treating "nothing applied" as proof that revert is safe.
    #[serde(default)]
    pub proposal_failed: bool,
    pub outcome: RemediationOutcome,
}

/// One checkpointed execution of the remediation graph.
#[derive(Debug, Clone, Serialize)]
pub struct RemediationGraphRun {
    /// Terminal graph state, including the graph-selected outcome.
    pub state: RemediationState,
    /// Durable LangGraph checkpoint thread id for audit/replay.
    pub thread_id: String,
    /// Durable LangGraph checkpoint snapshots for audit/replay.
    pub checkpoints:
        Vec<crate::decision_graph_introspection::DecisionGraphCheckpointSnapshot<RemediationState>>,
    /// Per-channel checkpoint writes from LangGraph's write-history stream.
    pub write_history: Vec<crate::decision_graph_introspection::DecisionGraphWriteHistoryEntry>,
    /// Typed LangGraph stream parts emitted by this decision run.
    pub stream: Vec<crate::decision_graph_introspection::DecisionGraphStreamPart>,
    /// Compiled LangGraph topology that produced this run.
    pub topology: crate::decision_graph_introspection::DecisionGraphTopology,
}

/// Proof that a pre-apply remediation graph checkpoint authorized mutation.
#[derive(Debug, Clone)]
pub struct RemediationApplyAuthorization {
    thread_id: String,
    checkpoint_id: String,
}

impl RemediationApplyAuthorization {
    /// Durable remediation graph thread that authorized the apply step.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Durable pre-apply decision-node checkpoint that authorized mutation.
    #[must_use]
    pub fn checkpoint_id(&self) -> &str {
        &self.checkpoint_id
    }

    /// Stable audit reference for this concrete authorization checkpoint.
    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }
}

/// Proof that a remediation graph checkpoint authorized a last-resort revert.
#[derive(Debug, Clone)]
pub struct RemediationRevertAuthorization {
    identifier: String,
    thread_id: String,
    checkpoint_id: String,
    stage: RemediationStage,
}

impl RemediationRevertAuthorization {
    #[must_use]
    pub fn identifier(&self) -> &str {
        &self.identifier
    }

    /// Durable remediation graph thread that authorized the revert.
    #[must_use]
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// Durable decision-node checkpoint that authorized the revert.
    #[must_use]
    pub fn checkpoint_id(&self) -> &str {
        &self.checkpoint_id
    }

    /// Stable audit reference for this concrete authorization checkpoint.
    #[must_use]
    pub fn checkpoint_ref(&self) -> String {
        format!("{}#{}", self.thread_id, self.checkpoint_id)
    }

    #[must_use]
    pub fn stage(&self) -> RemediationStage {
        self.stage
    }
}

impl RemediationGraphRun {
    /// Convert a pre-apply `Apply` graph result into a mutation authorization.
    #[must_use]
    pub fn apply_authorization(&self) -> Result<Option<RemediationApplyAuthorization>, String> {
        if self.state.stage != RemediationStage::PreApply
            || self.state.outcome != RemediationOutcome::Apply
        {
            return Ok(None);
        }
        let checkpoint_id =
            crate::decision_graph_introspection::terminal_decision_checkpoint_result(
                "remediation",
                &self.thread_id,
                &self.state,
                &self.checkpoints,
                &self.write_history,
            )?
            .checkpoint_id
            .clone();
        Ok(Some(RemediationApplyAuthorization {
            thread_id: self.thread_id.clone(),
            checkpoint_id,
        }))
    }

    /// Convert a terminal `Revert` graph result into a last-resort revert authorization.
    #[must_use]
    pub fn revert_authorization(&self) -> Result<Option<RemediationRevertAuthorization>, String> {
        if self.state.outcome != RemediationOutcome::Revert {
            return Ok(None);
        }
        let checkpoint_id =
            crate::decision_graph_introspection::terminal_decision_checkpoint_result(
                "remediation",
                &self.thread_id,
                &self.state,
                &self.checkpoints,
                &self.write_history,
            )?
            .checkpoint_id
            .clone();
        Ok(Some(RemediationRevertAuthorization {
            identifier: self.state.identifier.clone(),
            thread_id: self.thread_id.clone(),
            checkpoint_id,
            stage: self.state.stage,
        }))
    }
}

const CLASSIFY: &str = "classify";
const VERIFY: &str = "verify";
const APPLY: &str = "apply";
const CLEAR: &str = "clear";
const ESCALATE: &str = "escalate";
const REVERT: &str = "revert";

fn node_config(node: &str, checkpointer_backend: &str, checkpointer_scope: &str) -> NodeConfig {
    NodeConfig::new()
        .with_metadata("sentinel.graph", "remediation")
        .with_metadata("sentinel.node", node)
        .with_metadata("sentinel.checkpointer_backend", checkpointer_backend)
        .with_metadata("sentinel.checkpointer_scope", checkpointer_scope)
        .with_timeout(NodeTimeoutPolicy::run_only(Duration::from_secs(2)))
}

fn remediation_state_schema() -> StateSchema<RemediationState> {
    StateSchema::new()
        .with_serializable_validation()
        .with_json_schema(serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": [
                "identifier",
                "stage",
                "missing_before",
                "planned_count",
                "applied_count",
                "still_missing",
                "proposal_failed",
                "outcome"
            ],
            "properties": {
                "identifier": { "type": "string", "minLength": 1 },
                "stage": { "type": "string", "enum": ["PreApply", "PostApply"] },
                "missing_before": {
                    "type": "array",
                    "items": { "type": "string", "minLength": 1 }
                },
                "planned_count": { "type": "integer", "minimum": 0 },
                "applied_count": { "type": "integer", "minimum": 0 },
                "still_missing": {
                    "type": "array",
                    "items": { "type": "string", "minLength": 1 }
                },
                "proposal_failed": { "type": "boolean" },
                "outcome": {
                    "type": "string",
                    "enum": ["Apply", "Clear", "Escalate", "Revert"]
                }
            },
            "x-sentinel": {
                "graph": "remediation",
                "authority": "langgraph"
            }
        }))
        .with_validator(|state: &RemediationState| {
            if state.identifier.trim().is_empty() {
                return Err(StateError::ValidationFailed(
                    "remediation identifier must not be empty".to_string(),
                ));
            }
            if state.applied_count > state.planned_count {
                return Err(StateError::ValidationFailed(
                    "remediation applied_count cannot exceed planned_count".to_string(),
                ));
            }
            match state.outcome {
                RemediationOutcome::Apply => {
                    if state.stage != RemediationStage::PreApply {
                        return Err(StateError::ValidationFailed(
                            "remediation Apply is only valid for PreApply checkpoints".to_string(),
                        ));
                    }
                    if state.proposal_failed || state.planned_count == 0 {
                        return Err(StateError::ValidationFailed(
                            "remediation Apply requires a non-failed fillable plan".to_string(),
                        ));
                    }
                }
                RemediationOutcome::Revert => {
                    if state.still_missing.is_empty() {
                        return Err(StateError::ValidationFailed(
                            "remediation Revert requires remaining missing fields".to_string(),
                        ));
                    }
                }
                RemediationOutcome::Clear | RemediationOutcome::Escalate => {}
            }
            Ok(())
        })
}

/// Build the remediation decision graph with a durable env-selected checkpointer.
/// The external I/O (propose/apply/re-fetch) runs in the orchestrator, but the
/// graph authorizes the mutation before `apply_remediation` runs:
///
/// - `PreApply` + fillable plan -> `Apply` (orchestrator may mutate Linear).
/// - `PreApply` + proposal failure -> `Escalate`.
/// - `PreApply` + no fillable plan -> `Revert`.
/// - `PostApply` verifies the result: ready -> `Clear`; partial -> `Escalate`;
///   no mutation and gaps -> `Revert`.
///
/// # Errors
/// Returns the stringified `langgraph-core` build/compile error on failure.
pub async fn build_remediation_graph(
) -> Result<langgraph_core::application::services::CompilationResult<RemediationState>, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_graph("remediation").await?;
    build_remediation_graph_with_checkpointer(checkpointer).await
}

/// Build the remediation graph with an ephemeral SQLite checkpointer.
#[cfg(test)]
async fn build_remediation_graph_with_ephemeral_sqlite(
) -> Result<langgraph_core::application::services::CompilationResult<RemediationState>, String> {
    build_remediation_graph_with_database_path(":memory:").await
}

/// Build the remediation graph against a caller-supplied SQLite checkpoint DB.
#[cfg(test)]
async fn build_remediation_graph_with_database_path(
    database_path: &str,
) -> Result<langgraph_core::application::services::CompilationResult<RemediationState>, String> {
    let checkpointer = crate::decision_graph_store::checkpointer_for_config(
        crate::decision_graph_store::DecisionGraphCheckpointerConfig::Sqlite {
            database_path: database_path.to_string(),
        },
    )
    .await?;
    build_remediation_graph_with_checkpointer(checkpointer).await
}

async fn build_remediation_graph_with_checkpointer(
    checkpointer: crate::decision_graph_store::DecisionGraphCheckpointer,
) -> Result<langgraph_core::application::services::CompilationResult<RemediationState>, String> {
    let checkpointer_backend = checkpointer.backend();
    let checkpointer_scope = checkpointer.scope();
    let schema = remediation_state_schema();
    let builder = StateGraphBuilder::<RemediationState>::with_schema(schema.clone())
        .with_input_schema(schema.clone())
        .with_output_schema(schema)
        .add_async_node_with_config(
            CLASSIFY,
            |s: RemediationState| async move {
                crate::decision_graph_introspection::emit_decision_node_event(
                    "remediation",
                    CLASSIFY,
                    &s.identifier,
                )?;
                Ok::<_, NodeError>(s)
            },
            node_config(CLASSIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            VERIFY,
            |s: RemediationState| async move {
                crate::decision_graph_introspection::emit_decision_node_event(
                    "remediation",
                    VERIFY,
                    &s.identifier,
                )?;
                Ok::<_, NodeError>(s)
            },
            node_config(VERIFY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            APPLY,
            |s: RemediationState| async move {
                crate::decision_graph_introspection::emit_decision_node_event(
                    "remediation",
                    APPLY,
                    &s.identifier,
                )?;
                let mut n = s;
                n.outcome = RemediationOutcome::Apply;
                Ok::<_, NodeError>(n)
            },
            node_config(APPLY, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            CLEAR,
            |s: RemediationState| async move {
                crate::decision_graph_introspection::emit_decision_node_event(
                    "remediation",
                    CLEAR,
                    &s.identifier,
                )?;
                let mut n = s;
                n.outcome = RemediationOutcome::Clear;
                Ok::<_, NodeError>(n)
            },
            node_config(CLEAR, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            ESCALATE,
            |s: RemediationState| async move {
                crate::decision_graph_introspection::emit_decision_node_event(
                    "remediation",
                    ESCALATE,
                    &s.identifier,
                )?;
                let mut n = s;
                n.outcome = RemediationOutcome::Escalate;
                Ok::<_, NodeError>(n)
            },
            node_config(ESCALATE, checkpointer_backend, checkpointer_scope),
        )
        .add_async_node_with_config(
            REVERT,
            |s: RemediationState| async move {
                crate::decision_graph_introspection::emit_decision_node_event(
                    "remediation",
                    REVERT,
                    &s.identifier,
                )?;
                let mut n = s;
                n.outcome = RemediationOutcome::Revert;
                Ok::<_, NodeError>(n)
            },
            node_config(REVERT, checkpointer_backend, checkpointer_scope),
        )
        .add_edge(START, CLASSIFY)
        .add_edge(CLASSIFY, VERIFY)
        .add_conditional_edge(VERIFY, |s: &RemediationState| match s.stage {
            RemediationStage::PreApply => {
                if s.proposal_failed {
                    ESCALATE.into()
                } else if s.planned_count > 0 {
                    APPLY.into()
                } else if s.still_missing.is_empty() {
                    CLEAR.into()
                } else {
                    REVERT.into()
                }
            }
            RemediationStage::PostApply => {
                if s.still_missing.is_empty() {
                    CLEAR.into()
                } else if s.proposal_failed {
                    ESCALATE.into()
                } else if s.applied_count > 0 {
                    ESCALATE.into()
                } else {
                    REVERT.into()
                }
            }
        })
        .add_edge(APPLY, END)
        .add_edge(CLEAR, END)
        .add_edge(ESCALATE, END)
        .add_edge(REVERT, END);

    let graph = builder.build().map_err(|e| e.to_string())?;
    GraphCompiler::new()
        .with_checkpointer(checkpointer.into_saver())
        .compile_with_config(graph)
        .map_err(|e| e.to_string())
}

/// Run the remediation graph over a seeded state and return the graph run.
///
/// # Errors
/// Returns the stringified graph execution error on failure.
pub async fn run_remediation_decision_report(
    compiled: &langgraph_core::application::services::CompilationResult<RemediationState>,
    state: RemediationState,
) -> Result<RemediationGraphRun, String> {
    let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
        compiled,
        "remediation",
        &state.identifier,
        &state,
    )?;
    let identifier = state.identifier.clone();
    let streamed = crate::decision_graph_introspection::stream_decision_run(
        compiled,
        &thread_id,
        "remediation",
        &identifier,
        state,
    )
    .await?;
    let checkpoints =
        crate::decision_graph_introspection::checkpoint_history(compiled, &thread_id).await?;
    let write_history =
        crate::decision_graph_introspection::write_history(compiled, &thread_id, None).await?;
    crate::decision_graph_introspection::validate_decision_graph_run(
        "remediation",
        &thread_id,
        &streamed.state,
        &streamed.stream,
        &checkpoints,
        &write_history,
    )?;
    Ok(RemediationGraphRun {
        state: streamed.state,
        thread_id,
        checkpoints,
        write_history,
        stream: streamed.stream,
        topology: remediation_graph_topology(compiled)?,
    })
}

/// Reflect the compiled remediation graph topology.
pub fn remediation_graph_topology(
    compiled: &langgraph_core::application::services::CompilationResult<RemediationState>,
) -> Result<crate::decision_graph_introspection::DecisionGraphTopology, String> {
    crate::decision_graph_introspection::topology("remediation", compiled)
}

/// Run the remediation graph over a seeded state and return the outcome.
///
/// # Errors
/// Returns the stringified graph execution error on failure.
#[cfg(test)]
pub async fn run_remediation_decision(
    compiled: &langgraph_core::application::services::CompilationResult<RemediationState>,
    state: RemediationState,
) -> Result<RemediationOutcome, String> {
    Ok(run_remediation_decision_report(compiled, state)
        .await?
        .state
        .outcome)
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
        assert_eq!(
            LabelCatalog::resolve(&c.type_labels, "bug"),
            Some("bug-id".into())
        );
        assert_eq!(
            LabelCatalog::resolve(&c.type_labels, " Feature "),
            Some("feat-id".into())
        );
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
        assert!(
            p.contains("Bug, Feature, Enhancement"),
            "closed Type set must be listed"
        );
        assert!(
            p.contains("unfillable"),
            "must instruct to escalate, not guess"
        );
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
        assert!(!plan.proposal_failed);
        assert_eq!(plan.unfillable, vec!["priority".to_string()]);
        assert!(plan.acceptance_criteria.unwrap().contains("- [ ]"));
    }

    #[test]
    fn parse_plan_rejects_bad_priority() {
        let plan = parse_plan(
            "{\"priority\":9,\"unfillable\":[],\"rationale\":\"bad priority\"}",
            &["priority".into()],
        );
        assert!(plan.proposal_failed);
        assert_eq!(plan.priority, None);
        assert_eq!(plan.unfillable, vec!["priority".to_string()]);
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
        assert_eq!(
            plan.type_label.as_deref(),
            Some("Bug"),
            "type_label must survive braces in a later string"
        );
        assert!(plan
            .acceptance_criteria
            .unwrap()
            .contains("${TEST_DATABASE_URL}"));
    }

    #[test]
    fn parse_plan_unparseable_escalates_everything() {
        let plan = parse_plan(
            "the model rambled with no json",
            &["Type label".into(), "estimate".into()],
        );
        assert!(plan.type_label.is_none());
        assert!(plan.proposal_failed);
        assert_eq!(
            plan.unfillable,
            vec!["Type label".to_string(), "estimate".to_string()]
        );
    }

    #[test]
    fn applicable_change_count_counts_only_real_mutations() {
        let plan = RemediationPlan {
            type_label: Some("Bug".into()),
            area_label: Some("MissingArea".into()),
            estimate: Some(3),
            priority: Some(2),
            acceptance_criteria: Some("- [ ] one".into()),
            ..Default::default()
        };
        assert_eq!(
            plan.applicable_change_count(&catalog()),
            3,
            "unresolvable labels and non-mutated acceptance criteria do not authorize apply"
        );
    }

    #[test]
    fn parse_label_catalog_response_rejects_unverified_catalogs() {
        let catalog = parse_label_catalog_response(
            "FPCRM-catalog",
            r#"{"data":{"issue":{"team":{"labels":{"nodes":[
                {"id":"type-1","name":"Bug","parent":{"name":"Type"}},
                {"id":"area-1","name":"Settings","parent":{"name":"Area"}},
                {"id":"other-1","name":"Backend","parent":{"name":"Other"}}
            ]}}}}}"#,
        )
        .expect("catalog parses");
        assert_eq!(catalog.type_labels, vec![("Bug".into(), "type-1".into())]);
        assert_eq!(
            catalog.area_labels,
            vec![("Settings".into(), "area-1".into())]
        );

        assert!(parse_label_catalog_response(
            "FPCRM-catalog",
            r#"{"errors":[{"message":"denied"}],"data":null}"#
        )
        .expect_err("GraphQL errors must fail")
        .contains("GraphQL errors"));
        assert!(
            parse_label_catalog_response("FPCRM-catalog", r#"{"data":{"issue":{}}}"#)
                .expect_err("missing labels must fail")
                .contains("labels.nodes")
        );
        assert!(
            parse_label_catalog_response(
                "FPCRM-catalog",
                r#"{"data":{"issue":{"team":{"labels":{"nodes":[{"name":"Bug","parent":{"name":"Type"}}]}}}}}"#
            )
            .expect_err("missing label id must fail")
            .contains("without id")
        );
    }

    #[test]
    fn parse_ticket_text_response_rejects_unverified_ticket_text() {
        let (title, desc) = parse_ticket_text_response(
            "FPCRM-text",
            r#"{"data":{"issue":{"title":"Fix parser","description":null}}}"#,
        )
        .expect("ticket text parses");
        assert_eq!(title, "Fix parser");
        assert_eq!(desc, "");

        assert!(parse_ticket_text_response(
            "FPCRM-text",
            r#"{"errors":[{"message":"denied"}],"data":null}"#
        )
        .expect_err("GraphQL errors must fail")
        .contains("GraphQL errors"));
        assert!(parse_ticket_text_response(
            "FPCRM-text",
            r#"{"data":{"issue":{"description":"x"}}}"#
        )
        .expect_err("missing title must fail")
        .contains("title"));
    }

    #[test]
    fn linear_success_validator_rejects_failed_graphql_envelopes() {
        validate_linear_success(
            "issueUpdate",
            r#"{"data":{"issueUpdate":{"success":true}}}"#,
        )
        .expect("success envelope");
        assert!(validate_linear_success(
            "issueUpdate",
            r#"{"errors":[{"message":"nope"}],"data":null}"#
        )
        .expect_err("GraphQL errors must fail")
        .contains("GraphQL errors"));
        assert!(validate_linear_success(
            "issueUpdate",
            r#"{"data":{"issueUpdate":{"success":false}}}"#
        )
        .expect_err("success=false must fail")
        .contains("success=true"));
        assert!(
            validate_linear_success("issueUpdate", r#"{"data":{"issueUpdate":{}}}"#)
                .expect_err("missing success must fail")
                .contains("success=true")
        );
    }

    #[tokio::test]
    async fn apply_remediation_errors_when_graph_authorized_label_is_not_in_catalog() {
        let auth = RemediationApplyAuthorization {
            thread_id: "remediation:FPCRM-missing-label:abc".into(),
            checkpoint_id: "checkpoint-1".into(),
        };
        let plan = RemediationPlan {
            type_label: Some("MissingType".into()),
            ..Default::default()
        };
        let err = apply_remediation(
            &reqwest::Client::new(),
            "token",
            "issue-1",
            &plan,
            &catalog(),
            &auth,
        )
        .await
        .expect_err("missing catalog label must fail closed");
        assert!(err.contains("MissingType"));
    }

    #[tokio::test]
    async fn graph_authorizes_apply_before_linear_mutation() {
        let g = build_remediation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let s = RemediationState {
            identifier: "FPCRM-preapply".into(),
            stage: RemediationStage::PreApply,
            missing_before: vec!["Type label".into()],
            planned_count: 1,
            applied_count: 0,
            still_missing: vec![],
            proposal_failed: false,
            outcome: RemediationOutcome::Clear,
        };
        let run = run_remediation_decision_report(&g, s).await.unwrap();
        assert_eq!(
            run.state.outcome,
            RemediationOutcome::Apply,
            "pre-apply graph must explicitly authorize Linear mutation"
        );
        let auth = run
            .apply_authorization()
            .expect("Apply run should produce authorization token")
            .expect("authorization");
        assert_eq!(auth.thread_id(), run.thread_id);
        let auth_checkpoint = run
            .checkpoints
            .iter()
            .find(|checkpoint| checkpoint.checkpoint_id == auth.checkpoint_id())
            .expect("authorization checkpoint must be present");
        assert_eq!(auth_checkpoint.source_node.as_deref(), Some(APPLY));
        assert_eq!(
            auth_checkpoint
                .writes
                .iter()
                .find(|write| write.channel == "state")
                .expect("authorization checkpoint state write")
                .node_id
                .as_str(),
            APPLY
        );
        assert_eq!(auth.checkpoint_ref(), auth_checkpoint.checkpoint_ref());
    }

    #[tokio::test]
    async fn graph_blocks_apply_when_proposal_failed() {
        let g = build_remediation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let s = RemediationState {
            identifier: "FPCRM-preapply-failed".into(),
            stage: RemediationStage::PreApply,
            missing_before: vec!["Type label".into()],
            planned_count: 1,
            applied_count: 0,
            still_missing: vec!["Type label".into()],
            proposal_failed: true,
            outcome: RemediationOutcome::Clear,
        };
        assert_eq!(
            run_remediation_decision(&g, s).await.unwrap(),
            RemediationOutcome::Escalate,
            "proposal failure must escalate before any Linear mutation"
        );
    }

    #[tokio::test]
    async fn graph_does_not_authorize_apply_from_post_apply_checkpoint() {
        let g = build_remediation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let s = RemediationState {
            identifier: "FPCRM-postapply-no-auth".into(),
            stage: RemediationStage::PostApply,
            missing_before: vec!["Type label".into()],
            planned_count: 1,
            applied_count: 1,
            still_missing: vec![],
            proposal_failed: false,
            outcome: RemediationOutcome::Clear,
        };
        let run = run_remediation_decision_report(&g, s).await.unwrap();
        assert_eq!(run.state.outcome, RemediationOutcome::Clear);
        assert!(
            run.apply_authorization()
                .expect("authorization result")
                .is_none(),
            "post-apply verification checkpoints must not authorize mutation"
        );
    }

    #[tokio::test]
    async fn graph_reverts_preapply_when_nothing_can_be_filled() {
        let g = build_remediation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let s = RemediationState {
            identifier: "FPCRM-preapply-empty".into(),
            stage: RemediationStage::PreApply,
            missing_before: vec!["everything".into()],
            planned_count: 0,
            applied_count: 0,
            still_missing: vec!["everything".into()],
            proposal_failed: false,
            outcome: RemediationOutcome::Clear,
        };
        assert_eq!(
            run_remediation_decision(&g, s).await.unwrap(),
            RemediationOutcome::Revert,
            "no fillable plan should route to last-resort revert without applying"
        );
    }

    #[tokio::test]
    async fn graph_schema_rejects_forged_apply_authorization() {
        let g = build_remediation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let err = run_remediation_decision_report(
            &g,
            RemediationState {
                identifier: "FPCRM-forged-apply".into(),
                stage: RemediationStage::PreApply,
                missing_before: vec!["Type label".into()],
                planned_count: 0,
                applied_count: 0,
                still_missing: vec!["Type label".into()],
                proposal_failed: false,
                outcome: RemediationOutcome::Apply,
            },
        )
        .await
        .expect_err("forged Apply state must fail LangGraph schema validation");
        assert!(
            err.contains("remediation Apply requires"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn revert_authorization_exists_only_for_revert_outcome() {
        let g = build_remediation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let revert = run_remediation_decision_report(
            &g,
            RemediationState {
                identifier: "FPCRM-revert-auth".into(),
                stage: RemediationStage::PreApply,
                missing_before: vec!["everything".into()],
                planned_count: 0,
                applied_count: 0,
                still_missing: vec!["everything".into()],
                proposal_failed: false,
                outcome: RemediationOutcome::Clear,
            },
        )
        .await
        .unwrap();
        let auth = revert
            .revert_authorization()
            .expect("Revert run should authorize last-resort revert")
            .expect("authorization");
        assert_eq!(auth.identifier(), "FPCRM-revert-auth");
        assert_eq!(auth.thread_id(), revert.thread_id);
        let auth_checkpoint = revert
            .checkpoints
            .iter()
            .find(|checkpoint| checkpoint.checkpoint_id == auth.checkpoint_id())
            .expect("authorization checkpoint must be present");
        assert_eq!(auth_checkpoint.source_node.as_deref(), Some(REVERT));
        assert_eq!(
            auth_checkpoint
                .writes
                .iter()
                .find(|write| write.channel == "state")
                .expect("authorization checkpoint state write")
                .node_id
                .as_str(),
            REVERT
        );
        assert_eq!(auth.checkpoint_ref(), auth_checkpoint.checkpoint_ref());
        assert_eq!(auth.stage(), RemediationStage::PreApply);

        let clear = run_remediation_decision_report(
            &g,
            RemediationState {
                identifier: "FPCRM-no-revert-auth".into(),
                stage: RemediationStage::PostApply,
                missing_before: vec!["Type label".into()],
                planned_count: 1,
                applied_count: 1,
                still_missing: vec![],
                proposal_failed: false,
                outcome: RemediationOutcome::Clear,
            },
        )
        .await
        .unwrap();
        assert!(
            clear
                .revert_authorization()
                .expect("authorization result")
                .is_none(),
            "Clear run must not authorize a Linear revert"
        );
    }

    #[tokio::test]
    async fn graph_clears_when_fully_remediated() {
        let g = build_remediation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let s = RemediationState {
            identifier: "FPCRM-596".into(),
            stage: RemediationStage::PostApply,
            missing_before: vec!["Type label".into()],
            planned_count: 1,
            applied_count: 1,
            still_missing: vec![],
            proposal_failed: false,
            outcome: RemediationOutcome::Clear,
        };
        assert_eq!(
            run_remediation_decision(&g, s).await.unwrap(),
            RemediationOutcome::Clear
        );
    }

    #[tokio::test]
    async fn graph_escalates_on_partial_remediation() {
        let g = build_remediation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let s = RemediationState {
            identifier: "X-1".into(),
            stage: RemediationStage::PostApply,
            missing_before: vec!["Type label".into(), "acceptance criteria".into()],
            planned_count: 1,
            applied_count: 1,                                  // filled the label
            still_missing: vec!["acceptance criteria".into()], // but AC needs a human
            proposal_failed: false,
            outcome: RemediationOutcome::Clear,
        };
        assert_eq!(
            run_remediation_decision(&g, s).await.unwrap(),
            RemediationOutcome::Escalate
        );
    }

    #[tokio::test]
    async fn graph_reverts_when_nothing_fillable() {
        let g = build_remediation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let s = RemediationState {
            identifier: "X-2".into(),
            stage: RemediationStage::PostApply,
            missing_before: vec!["everything".into()],
            planned_count: 0,
            applied_count: 0, // couldn't fill anything
            still_missing: vec!["everything".into()],
            proposal_failed: false,
            outcome: RemediationOutcome::Clear,
        };
        assert_eq!(
            run_remediation_decision(&g, s).await.unwrap(),
            RemediationOutcome::Revert
        );
    }

    #[tokio::test]
    async fn graph_escalates_when_proposal_failed() {
        let g = build_remediation_graph_with_ephemeral_sqlite()
            .await
            .unwrap();
        let s = RemediationState {
            identifier: "X-proposal-failed".into(),
            stage: RemediationStage::PostApply,
            missing_before: vec!["everything".into()],
            planned_count: 0,
            applied_count: 0,
            still_missing: vec!["everything".into()],
            proposal_failed: true,
            outcome: RemediationOutcome::Clear,
        };
        assert_eq!(
            run_remediation_decision(&g, s).await.unwrap(),
            RemediationOutcome::Escalate,
            "proposal failure must not be treated as proof that revert is safe"
        );
    }

    #[tokio::test]
    async fn graph_persists_checkpoint_history_to_sqlite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("remediation.db");
        let db = db.to_string_lossy().to_string();

        let g = build_remediation_graph_with_database_path(&db)
            .await
            .expect("graph builds");
        let s = RemediationState {
            identifier: "FPCRM-remediate-durable".into(),
            stage: RemediationStage::PostApply,
            missing_before: vec!["Type label".into()],
            planned_count: 1,
            applied_count: 1,
            still_missing: vec![],
            proposal_failed: false,
            outcome: RemediationOutcome::Clear,
        };
        let thread_id = crate::decision_graph_store::run_thread_id_for_compiled(
            &g,
            "remediation",
            &s.identifier,
            &s,
        )
        .expect("thread id");
        let run = run_remediation_decision_report(&g, s).await.expect("runs");
        assert_eq!(run.thread_id, thread_id);
        assert_eq!(run.state.outcome, RemediationOutcome::Clear);
        assert!(!run.checkpoints.is_empty(), "run must expose checkpoints");
        assert_eq!(
            run.checkpoints.first().expect("latest").thread_id,
            thread_id
        );
        assert_eq!(
            run.checkpoints.first().expect("latest").state.outcome,
            RemediationOutcome::Clear
        );
        assert!(!run.stream.is_empty(), "run must expose stream parts");
        assert!(
            run.stream
                .iter()
                .any(|part| part.event_type == "ExecutionComplete"),
            "stream must expose LangGraph execution completion"
        );
        assert!(
            run.stream.iter().any(|part| part.payload_kind == "values"),
            "stream must expose LangGraph values payloads"
        );
        assert!(
            run.stream
                .iter()
                .any(|part| part.payload_kind == "checkpoints"),
            "stream must expose LangGraph checkpoint payloads"
        );
        assert!(
            run.stream.iter().any(|part| {
                part.payload_kind == "custom"
                    && part.payload_json["type"] == "sentinel.decision_node"
                    && part.payload_json["graph"] == "remediation"
            }),
            "stream must expose Sentinel custom decision-node payloads"
        );
        assert!(
            run.write_history
                .iter()
                .any(|write| write.channel == "state"),
            "run must expose state channel write history"
        );
        assert!(
            run.write_history
                .iter()
                .all(|write| write.value_len > 0 && write.value_sha256.len() == 64),
            "write history must expose value length and sha256"
        );
        assert!(
            run.write_history
                .iter()
                .filter(|write| write.channel == "state")
                .any(|write| write.value_json["outcome"] == "Clear"),
            "state write history must decode the terminal outcome JSON"
        );
        assert_eq!(run.topology.graph, "remediation");
        assert!(run.topology.durable_checkpointer);
        assert_eq!(run.topology.checkpointer_backend, "sqlite");
        assert!(
            run.topology
                .checkpointer_scope
                .starts_with("database_path:"),
            "topology must expose sanitized checkpoint scope"
        );
        assert_eq!(
            run.topology.schemas.state.as_ref().expect("state schema")["x-sentinel"]["graph"],
            "remediation"
        );
        assert!(run.topology.schemas.input.is_some());
        assert!(run.topology.schemas.output.is_some());
        assert!(
            run.topology
                .nodes
                .iter()
                .all(|node| node.has_timeout_policy),
            "every remediation graph node should carry a timeout policy"
        );
        assert!(
            run.topology.nodes.iter().any(|node| {
                node.id == VERIFY
                    && node.metadata.get("sentinel.graph").map(String::as_str)
                        == Some("remediation")
                    && node
                        .metadata
                        .get("sentinel.checkpointer_backend")
                        .map(String::as_str)
                        == Some("sqlite")
                    && node
                        .metadata
                        .get("sentinel.checkpointer_scope")
                        .is_some_and(|scope| scope.starts_with("database_path:"))
            }),
            "topology must expose remediation node metadata"
        );
        assert!(
            run.topology
                .edges
                .iter()
                .any(|edge| edge.kind == "conditional"),
            "topology must expose conditional routing"
        );

        let rebuilt = build_remediation_graph_with_database_path(&db)
            .await
            .expect("graph rebuilds");
        let history = rebuilt
            .get_state_history(&thread_id)
            .await
            .expect("history");
        assert!(!history.is_empty(), "checkpoint history must persist");
        assert_eq!(
            history.first().expect("latest").state().outcome,
            RemediationOutcome::Clear
        );
    }

    #[tokio::test]
    async fn graph_rerun_same_ticket_uses_fresh_thread_for_changed_input() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("remediation-rerun.db");
        let db = db.to_string_lossy().to_string();
        let graph = build_remediation_graph_with_database_path(&db)
            .await
            .expect("graph builds");

        let first = RemediationState {
            identifier: "FPCRM-remediate-rerun".into(),
            stage: RemediationStage::PostApply,
            missing_before: vec!["Type label".into()],
            planned_count: 1,
            applied_count: 1,
            still_missing: vec![],
            proposal_failed: false,
            outcome: RemediationOutcome::Clear,
        };
        assert_eq!(
            run_remediation_decision(&graph, first)
                .await
                .expect("first run"),
            RemediationOutcome::Clear
        );

        let second = RemediationState {
            identifier: "FPCRM-remediate-rerun".into(),
            stage: RemediationStage::PostApply,
            missing_before: vec!["everything".into()],
            planned_count: 0,
            applied_count: 0,
            still_missing: vec!["everything".into()],
            proposal_failed: false,
            outcome: RemediationOutcome::Clear,
        };
        assert_eq!(
            run_remediation_decision(&graph, second)
                .await
                .expect("second run"),
            RemediationOutcome::Revert,
            "changed remediation facts for the same ticket must not resume the stale clear checkpoint"
        );
    }
}
