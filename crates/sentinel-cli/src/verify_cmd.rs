//! `sentinel verify` — Verify a session's proof chain

use anyhow::Result;
use colored::Colorize;

pub fn run(session: &str) -> Result<()> {
    println!("{}", "Sentinel Proof Chain Verification".bold());
    println!("Session: {session}\n");

    // Load proof chain
    let chain = sentinel_infrastructure::proof_store::load_chain(session)?;

    match chain {
        None => {
            println!("{}", "No proof chain found for this session.".yellow());
        }
        Some(chain) => {
            let verification = chain.verify();

            if verification.valid {
                println!(
                    "{} Chain is valid! {} phases verified.",
                    "✓".green().bold(),
                    verification.phases_verified
                );
            } else {
                println!("{} Chain verification FAILED!", "✗".red().bold());
                for error in &verification.errors {
                    println!("  {} {error}", "•".red());
                }
            }

            // Ed25519 signature verification (fail closed) when a public key is
            // configured. Hash verification above proves the chain is internally
            // consistent; this proves the signed entries were actually signed by
            // the holder of SENTINEL_VERIFY_KEY and not forged.
            match crate::mcp_cmd::load_verify_key_from_env() {
                Some(key) => {
                    let signing_required = matches!(
                        std::env::var("SENTINEL_SIGNING_REQUIRED").ok().as_deref(),
                        Some("1" | "true" | "TRUE" | "yes")
                    );
                    let report = chain.verify_signatures(&key, signing_required);
                    if report.is_ok() {
                        println!(
                            "{} Signatures verified: {} signed, {} unsigned.",
                            "✓".green().bold(),
                            report.verified,
                            report.unsigned
                        );
                    } else {
                        println!("{} Signature verification FAILED!", "✗".red().bold());
                        for entry_id in &report.failures {
                            println!("  {} bad/absent signature on entry {entry_id}", "•".red());
                        }
                    }
                },
                None => {
                    println!(
                        "{} Signatures NOT verified (set SENTINEL_VERIFY_KEY to the Ed25519 public key).",
                        "⚠".yellow()
                    );
                },
            }

            // Print chain summary
            println!("\n{}", "Proof Chain:".bold());
            for (i, proof) in chain.proofs.iter().enumerate() {
                let status = if proof.judge_verdict.sufficient {
                    "✓".green()
                } else {
                    "✗".red()
                };
                println!(
                    "  {status} Phase {i}: {} (tessera: {}...)",
                    proof.phase_id.cyan(),
                    &proof.combined_hash[..12]
                );
                println!(
                    "    Judge: {} (confidence: {:.0}%)",
                    proof.judge_model,
                    proof.judge_verdict.confidence * 100.0
                );
            }
        }
    }

    Ok(())
}
