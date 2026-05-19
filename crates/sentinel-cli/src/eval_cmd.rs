//! `sentinel eval` — A12 external-benchmark CLI.
//!
//! Phase 2 ships the `list` subcommand. Future phases add the
//! benchmark runner (load cases → dispatch via A2 router → score
//! → emit results).

use std::path::PathBuf;

use anyhow::Result;

use sentinel_infrastructure::eval_corpus::{CorpusEntry, FilesystemEvalCorpus};

/// `sentinel eval list` — render every `case_id` in the BA-Eval
/// corpus. Output includes both the public and private test splits,
/// with the `is_private_test` flag clearly marked so operators see
/// which cases are held-back per spec §3.4.
pub fn list(json: bool, dir_override: Option<String>) -> Result<()> {
    let corpus = build_corpus(dir_override)?;
    let entries = corpus.list_case_ids()?;
    if json {
        render_json(&entries);
    } else {
        render_table(corpus.base_dir(), &entries);
    }
    Ok(())
}

fn build_corpus(dir_override: Option<String>) -> Result<FilesystemEvalCorpus> {
    dir_override.map_or_else(FilesystemEvalCorpus::with_default_path, |dir| {
        Ok(FilesystemEvalCorpus::at_dir(PathBuf::from(dir)))
    })
}

fn render_json(entries: &[CorpusEntry]) {
    let payload: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "case_id": e.case_id.as_str(),
                "is_private_test": e.is_private_test,
            })
        })
        .collect();
    let out = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "[]".to_string());
    println!("{out}");
}

fn render_table(base_dir: &std::path::Path, entries: &[CorpusEntry]) {
    if entries.is_empty() {
        println!("No cases registered under {}", base_dir.display());
        println!();
        println!(
            "To populate the corpus, drop EvalCase JSON files into:\n  \
             {}/cases/{{case_id}}.json\n  \
             {}/private-test-split/{{case_id}}.json",
            base_dir.display(),
            base_dir.display(),
        );
        return;
    }
    let public_count = entries.iter().filter(|e| !e.is_private_test).count();
    let private_count = entries.len() - public_count;
    println!(
        "BA-Eval corpus at {} — {} cases ({} public, {} private)",
        base_dir.display(),
        entries.len(),
        public_count,
        private_count,
    );
    println!();
    println!("  {:<40}  split", "case_id");
    println!("  {:-<40}  --------", "");
    for e in entries {
        let split = if e.is_private_test { "private" } else { "public" };
        println!("  {:<40}  {split}", e.case_id.as_str());
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sentinel_domain::eval::{
        CaseProvenance, EvalCase, EvalCaseId, GoldArtifact, ScoringRubric, SourceCorpus,
    };
    use std::fs;
    use tempfile::TempDir;

    fn write_case(corpus_dir: &std::path::Path, case_id: &str, is_private: bool) {
        let split_dir = if is_private {
            corpus_dir.join("private-test-split")
        } else {
            corpus_dir.join("cases")
        };
        fs::create_dir_all(&split_dir).unwrap();
        let case = EvalCase {
            case_id: EvalCaseId::new(case_id).unwrap(),
            stakeholder_brief: "test".into(),
            source_corpus: SourceCorpus::Public {
                url: "https://example.com".into(),
                license: "CC-BY-4.0".into(),
            },
            gold_artifact: Some(GoldArtifact {
                text: "gold".into(),
                author: "tester".into(),
                content_hash: None,
            }),
            gold_outcomes: None,
            scoring_rubric: ScoringRubric::ba_default(),
            provenance: CaseProvenance {
                contributor: "tester".into(),
                license: "CC-BY-4.0".into(),
                is_private_test: is_private,
            },
        };
        let path = split_dir.join(format!("{case_id}.json"));
        fs::write(&path, serde_json::to_string(&case).unwrap()).unwrap();
    }

    #[test]
    fn list_with_dir_override_succeeds_on_empty_corpus() {
        let dir = TempDir::new().unwrap();
        let result = list(false, Some(dir.path().to_string_lossy().into_owned()));
        assert!(result.is_ok());
    }

    #[test]
    fn list_with_populated_public_corpus_succeeds() {
        let dir = TempDir::new().unwrap();
        write_case(dir.path(), "case-A", false);
        write_case(dir.path(), "case-B", false);
        let result = list(false, Some(dir.path().to_string_lossy().into_owned()));
        assert!(result.is_ok());
    }

    #[test]
    fn list_with_private_split_succeeds() {
        let dir = TempDir::new().unwrap();
        write_case(dir.path(), "case-X", true);
        let result = list(false, Some(dir.path().to_string_lossy().into_owned()));
        assert!(result.is_ok());
    }

    #[test]
    fn list_json_mode_succeeds() {
        let dir = TempDir::new().unwrap();
        write_case(dir.path(), "case-A", false);
        write_case(dir.path(), "case-X", true);
        let result = list(true, Some(dir.path().to_string_lossy().into_owned()));
        assert!(result.is_ok());
    }

    #[test]
    fn build_corpus_with_override_uses_override_path() {
        let dir = TempDir::new().unwrap();
        let corpus = build_corpus(Some(dir.path().to_string_lossy().into_owned())).unwrap();
        assert_eq!(corpus.base_dir(), dir.path());
    }

    #[test]
    fn build_corpus_without_override_uses_default_path() {
        let corpus = build_corpus(None).unwrap();
        let p = corpus.base_dir().display().to_string();
        assert!(p.contains(".claude") && p.contains("ba-corpus"));
    }
}
