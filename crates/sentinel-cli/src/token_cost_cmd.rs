//! `sentinel token-cost scan` — price SEN-7 token aggregates at per-model
//! API rates and report the cached-vs-uncached split + cache savings. See
//! `sentinel-application::token_cost`.

use anyhow::{Context, Result};
use colored::Colorize;

use sentinel_application::token_cost::scan_token_cost;

pub async fn run() -> Result<()> {
    let metrics = sentinel_infrastructure::paths::sentinel_root().join("metrics");
    let tokens_input = metrics.join("tokens-per-ticket.jsonl");
    let output = metrics.join("token-cost.json");
    let graph_runs = output.with_extension("graph-runs.jsonl");

    println!(
        "{}",
        "Sentinel Token Cost (per-model, cached vs uncached)".bold()
    );
    println!("Tokens input:   {}", tokens_input.display());
    println!("Output summary: {}", output.display());
    println!("Graph audit:    {}", graph_runs.display());
    println!();

    let s = scan_token_cost(&tokens_input, &output).context("scan_token_cost failed")?;
    let graph_audit = crate::token_cost_graph::run_token_cost_graph_audit(&s, &graph_runs)
        .await
        .context("token cost graph audit failed")?;

    if s.tickets == 0 {
        println!(
            "{}",
            "No token data found at tokens-per-ticket.jsonl. Run `sentinel tokens scan` first."
                .yellow()
        );
        return Ok(());
    }

    println!("{}", "==== TOKEN COST ====".bold());
    println!("  {} tickets · {:.2}B tokens", s.tickets, b(s.total_tokens));
    println!(
        "  graph decision {} · {}",
        graph_audit.decision.bold(),
        graph_audit
            .authorization_checkpoint
            .as_deref()
            .expect("token cost graph audit requires checkpoint")
            .dimmed()
    );
    println!(
        "  input {:.1}M · output {:.1}M · cache-write {:.2}B · cache-read {:.2}B",
        m(s.input_tokens),
        m(s.output_tokens),
        b(s.cache_write_tokens),
        b(s.cache_read_tokens),
    );
    println!();
    println!(
        "  {}  ${:>12}",
        "WITH caching (real):  ".bold(),
        fmt_usd(s.cost_with_caching_usd).green().bold()
    );
    println!(
        "  {}  ${:>12}",
        "WITHOUT caching:      ".bold(),
        fmt_usd(s.cost_without_caching_usd)
    );
    println!(
        "  {}  ${:>12}  ({:.0}% cheaper)",
        "→ Caching saved:      ".bold(),
        fmt_usd(s.cache_savings_usd).green().bold(),
        s.cache_savings_fraction * 100.0
    );
    println!();

    println!("{}", "By model:".bold());
    for (model, c) in &s.by_model {
        println!(
            "  {model:8} {:.2}B tok · ${} cached",
            b(c.tokens),
            fmt_usd(c.cached_usd)
        );
    }
    if s.unknown_model_tokens > 0 {
        println!(
            "{}",
            format!(
                "  note: {:.2}B tokens had an unknown model and were not priced; graph decision should be unknown-model-risk.",
                b(s.unknown_model_tokens)
            )
            .dimmed()
        );
    }

    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn b(t: u64) -> f64 {
    t as f64 / 1e9
}
#[allow(clippy::cast_precision_loss)]
fn m(t: u64) -> f64 {
    t as f64 / 1e6
}

/// Format a dollar amount with thousands separators (no cents above $1k).
fn fmt_usd(v: f64) -> String {
    if v >= 1000.0 {
        let whole = v.round() as i64;
        let s = whole.abs().to_string();
        let mut out = String::new();
        for (i, ch) in s.chars().enumerate() {
            if i > 0 && (s.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(ch);
        }
        out
    } else {
        format!("{v:.2}")
    }
}
