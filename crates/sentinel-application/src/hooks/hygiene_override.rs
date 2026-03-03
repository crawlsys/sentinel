//! Hygiene Override Hook
//!
//! Runs on UserPromptSubmit. Checks user prompt for override patterns
//! (e.g., "override hygiene", "skip tests"). If matched, writes temporary
//! override files with 5-minute expiry so that git-hygiene-gate and
//! verification-gate will allow tool calls through.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;
use sentinel_domain::events::{HookInput, HookOutput};

/// Override file paths (in temp dir)
fn hygiene_override_path() -> PathBuf {
    std::env::temp_dir().join("claude-hygiene-override")
}

fn verification_override_path() -> PathBuf {
    std::env::temp_dir().join("claude-verification-override")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Check if prompt matches hygiene override patterns
fn is_hygiene_override(prompt: &str) -> bool {
    let patterns = [
        r"override\s+(hygiene|git|commit)",
        r"hygiene\s+override",
        r"force\s+continue",
        r"skip\s+hygiene",
    ];
    patterns.iter().any(|p| {
        Regex::new(p)
            .map(|re| re.is_match(prompt))
            .unwrap_or(false)
    })
}

/// Check if prompt matches verification override patterns
fn is_verification_override(prompt: &str) -> bool {
    let patterns = [
        r"override\s+verification",
        r"verification\s+override",
        r"skip\s+verification",
        r"skip\s+tests?",
        r"override\s+test",
    ];
    patterns.iter().any(|p| {
        Regex::new(p)
            .map(|re| re.is_match(prompt))
            .unwrap_or(false)
    })
}

/// Write a temporary override file with the current timestamp
fn write_override(path: &PathBuf) -> Result<(), std::io::Error> {
    fs::write(path, now_secs().to_string())
}

/// Process the hygiene-override hook event
pub fn process(input: &HookInput) -> HookOutput {
    let prompt = match &input.prompt {
        Some(p) => p.to_lowercase(),
        None => return HookOutput::allow(),
    };

    let hygiene = is_hygiene_override(&prompt);
    let verification = is_verification_override(&prompt);

    if hygiene {
        if let Err(e) = write_override(&hygiene_override_path()) {
            eprintln!("Failed to set hygiene override: {e}");
            return HookOutput::allow();
        }
        eprintln!(
            "\
+-------------------------------------------------------------+\n\
|  GIT HYGIENE OVERRIDE ACTIVATED                             |\n\
+-------------------------------------------------------------+\n\
|  Edit/Write tools unblocked for 5 minutes.                  |\n\
|                                                             |\n\
|  Remember to commit your changes!                           |\n\
|  The gate will re-engage after timeout or next commit.      |\n\
+-------------------------------------------------------------+"
        );
    }

    if verification {
        if let Err(e) = write_override(&verification_override_path()) {
            eprintln!("Failed to set verification override: {e}");
            return HookOutput::allow();
        }
        eprintln!(
            "\
+-------------------------------------------------------------+\n\
|  VERIFICATION OVERRIDE ACTIVATED                            |\n\
+-------------------------------------------------------------+\n\
|  git commit/push unblocked for 5 minutes.                   |\n\
|                                                             |\n\
|  Run tests before your next commit!                         |\n\
|  The gate will re-engage after timeout.                     |\n\
+-------------------------------------------------------------+"
        );
    }

    HookOutput::allow()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hygiene_override_patterns() {
        assert!(is_hygiene_override("override hygiene"));
        assert!(is_hygiene_override("override git"));
        assert!(is_hygiene_override("override commit"));
        assert!(is_hygiene_override("hygiene override"));
        assert!(is_hygiene_override("force continue"));
        assert!(is_hygiene_override("skip hygiene"));
    }

    #[test]
    fn test_hygiene_override_no_match() {
        assert!(!is_hygiene_override("hello world"));
        assert!(!is_hygiene_override("commit my changes"));
        assert!(!is_hygiene_override("what is hygiene"));
    }

    #[test]
    fn test_verification_override_patterns() {
        assert!(is_verification_override("override verification"));
        assert!(is_verification_override("verification override"));
        assert!(is_verification_override("skip verification"));
        assert!(is_verification_override("skip tests"));
        assert!(is_verification_override("skip test"));
        assert!(is_verification_override("override test"));
    }

    #[test]
    fn test_verification_override_no_match() {
        assert!(!is_verification_override("run the tests"));
        assert!(!is_verification_override("test everything"));
        assert!(!is_verification_override("verify my work"));
    }

    #[test]
    fn test_process_no_prompt() {
        let input = HookInput::default();
        let output = process(&input);
        assert!(output.blocked.is_none());
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_process_normal_prompt() {
        let input = HookInput {
            prompt: Some("just a normal message".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
        assert!(output.hook_specific_output.is_none());
    }

    #[test]
    fn test_process_hygiene_override_writes_file() {
        let input = HookInput {
            prompt: Some("override hygiene".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
        // The override file should exist in temp dir
        assert!(hygiene_override_path().exists());
        // Clean up
        let _ = fs::remove_file(hygiene_override_path());
    }

    #[test]
    fn test_process_verification_override_writes_file() {
        let input = HookInput {
            prompt: Some("skip tests".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
        assert!(verification_override_path().exists());
        // Clean up
        let _ = fs::remove_file(verification_override_path());
    }

    #[test]
    fn test_case_insensitive() {
        let input = HookInput {
            prompt: Some("Override Hygiene".to_string()),
            ..Default::default()
        };
        let output = process(&input);
        assert!(output.blocked.is_none());
        // Should match because prompt is lowercased
        assert!(hygiene_override_path().exists());
        let _ = fs::remove_file(hygiene_override_path());
    }
}
