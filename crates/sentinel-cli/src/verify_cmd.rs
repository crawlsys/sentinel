//! `sentinel verify` — Verify a session's proof chain

use anyhow::{bail, Result};
use colored::Colorize;

pub fn run(session: &str) -> Result<()> {
    println!("{}", "Sentinel Proof Chain Verification".bold());
    println!("Session: {session}\n");

    // Load proof chain
    let chain = sentinel_infrastructure::proof_store::load_chain(session)?;

    match chain {
        None => {
            println!("{}", "No proof chain found for this session.".yellow());
            bail!("no proof chain found for session '{session}'");
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
                bail!("proof chain verification failed for session '{session}'");
            }

            // Ed25519 signature verification is mandatory. Hash verification
            // above proves only internal consistency; signatures prove the
            // chain entries came from the configured Sentinel authority.
            let key = crate::mcp_cmd::load_verify_key_from_env()?;
            let report = chain.verify_signatures(&key);
            if report.is_ok() {
                println!(
                    "{} Signatures verified: {} signed entries.",
                    "✓".green().bold(),
                    report.verified
                );
            } else {
                println!("{} Signature verification FAILED!", "✗".red().bold());
                for entry_id in &report.failures {
                    println!("  {} bad/absent signature on entry {entry_id}", "•".red());
                }
                bail!("proof chain signature verification failed for session '{session}'");
            }

            // Print chain summary
            println!("\n{}", "Proof Chain:".bold());
            for (i, proof) in chain.phase_entries().enumerate() {
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
