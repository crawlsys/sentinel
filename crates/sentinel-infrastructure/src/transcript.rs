//! Session Transcript Reader
//!
//! Reads Claude Code JSONL transcripts for evidence extraction.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Find the transcript file for a session
pub fn find_transcript(session_id: &str) -> Result<Option<PathBuf>> {
    let projects_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join("projects");

    if !projects_dir.exists() {
        return Ok(None);
    }

    // Search all project directories for the session JSONL
    for entry in std::fs::read_dir(&projects_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let path = entry.path().join(format!("{session_id}.jsonl"));
        if path.exists() {
            return Ok(Some(path));
        }
    }

    Ok(None)
}

/// Extract execution log markers from a transcript
pub fn extract_log_markers(transcript_path: &std::path::Path) -> Result<Vec<String>> {
    let content = std::fs::read_to_string(transcript_path).context("Failed to read transcript")?;

    let markers: Vec<String> = content
        .lines()
        .filter(|line| {
            line.contains("[RUN]") || line.contains("[STEP ") || line.contains("[PHASE ")
        })
        .map(String::from)
        .collect();

    Ok(markers)
}
