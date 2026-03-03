//! Proof Store
//!
//! Persists proof chains to JSONL files for audit trails.
//! Each session gets its own proof file.

use std::path::PathBuf;

use anyhow::{Context, Result};

use sentinel_domain::proof::{PhaseProof, ProofChain};

/// Proof storage directory
fn proof_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("sentinel")
        .join("proofs")
}

/// Append a proof to the session's proof file (JSONL format)
pub fn append_proof(session_id: &str, proof: &PhaseProof) -> Result<()> {
    let dir = proof_dir();
    std::fs::create_dir_all(&dir)?;

    let path = dir.join(format!("{session_id}.jsonl"));
    let line = serde_json::to_string(proof)? + "\n";

    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(line.as_bytes())?;

    Ok(())
}

/// Save a complete proof chain
pub fn save_chain(chain: &ProofChain) -> Result<()> {
    let dir = proof_dir();
    std::fs::create_dir_all(&dir)?;

    let path = dir.join(format!("{}-chain.json", chain.session_id));
    let json = serde_json::to_string_pretty(chain)?;
    std::fs::write(&path, json)?;

    Ok(())
}

/// Load a proof chain
pub fn load_chain(session_id: &str) -> Result<Option<ProofChain>> {
    let path = proof_dir().join(format!("{session_id}-chain.json"));
    if !path.exists() {
        return Ok(None);
    }

    let json = std::fs::read_to_string(&path)?;
    let chain: ProofChain = serde_json::from_str(&json)?;
    Ok(Some(chain))
}

/// Load individual proofs from JSONL
pub fn load_proofs(session_id: &str) -> Result<Vec<PhaseProof>> {
    let path = proof_dir().join(format!("{session_id}.jsonl"));
    if !path.exists() {
        return Ok(vec![]);
    }

    let content = std::fs::read_to_string(&path)?;
    let mut proofs = Vec::new();
    for line in content.lines() {
        if !line.is_empty() {
            let proof: PhaseProof = serde_json::from_str(line)
                .context(format!("Failed to parse proof line: {}", &line[..line.len().min(80)]))?;
            proofs.push(proof);
        }
    }
    Ok(proofs)
}

/// List all sessions with proof chains
pub fn list_sessions() -> Result<Vec<String>> {
    let dir = proof_dir();
    if !dir.exists() {
        return Ok(vec![]);
    }

    let mut sessions = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(id) = name.strip_suffix("-chain.json") {
            sessions.push(id.to_string());
        }
    }
    Ok(sessions)
}
