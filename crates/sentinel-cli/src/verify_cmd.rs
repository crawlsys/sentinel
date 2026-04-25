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
