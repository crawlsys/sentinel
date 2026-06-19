use std::path::{Component, Path, PathBuf};

const RETIRED_CONTROL_PLANE_TERMS: &[&str] = &[
    concat!("con", "sul"),
    concat!("re", "public"),
    concat!("dash", "board"),
];

#[test]
fn retired_control_plane_surfaces_stay_removed() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let repo_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("crate must live under repo/crates/sentinel-infrastructure");

    let mut violations = Vec::new();
    scan_path(repo_root, repo_root, &mut violations);

    assert!(
        violations.is_empty(),
        "retired control-plane terms must stay out of Sentinel:\n{}",
        violations.join("\n")
    );
}

fn scan_path(repo_root: &Path, path: &Path, violations: &mut Vec<String>) {
    if should_skip(path) {
        return;
    }
    if path.is_dir() {
        let entries = std::fs::read_dir(path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        for entry in entries {
            let entry = entry.expect("failed to read directory entry");
            scan_path(repo_root, &entry.path(), violations);
        }
        return;
    }

    let rel_path = path.strip_prefix(repo_root).unwrap_or(path);
    for token in path_tokens(rel_path) {
        if RETIRED_CONTROL_PLANE_TERMS.contains(&token.as_str()) {
            violations.push(format!("{} path contains {token}", rel_path.display()));
        }
    }

    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    for (line_no, line) in content.lines().enumerate() {
        for token in text_tokens(line) {
            if RETIRED_CONTROL_PLANE_TERMS.contains(&token.as_str()) {
                violations.push(format!(
                    "{}:{} contains {token}",
                    rel_path.display(),
                    line_no + 1
                ));
            }
        }
    }
}

fn should_skip(path: &Path) -> bool {
    if matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(".git" | "target" | "Cargo.lock" | "enterprise_repository_boundaries.rs")
    ) {
        return true;
    }
    false
}

fn path_tokens(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => value.to_str(),
            _ => None,
        })
        .flat_map(text_tokens)
        .collect()
}

fn text_tokens(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .filter(|token| !token.is_empty())
        .map(|token| token.to_ascii_lowercase())
        .collect()
}
