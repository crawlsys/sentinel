//! Skill Store API Endpoints
//!
//! GET    /api/store/browse/:owner/:repo        — browse repo for skills
//! GET    /api/store/preview/:owner/:repo/:skill — preview SKILL.md
//! POST   /api/store/install                     — install skill from repo
//! DELETE /api/store/uninstall/:name             — remove skill from marketplace

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::{
    extract::Path as AxumPath,
    http::StatusCode,
    routing::{delete, get, post},
    Json, Router,
};
use sentinel_infrastructure::operational_api_read_graph::OperationalApiReadSurface;
use serde::{Deserialize, Serialize};

use sentinel_application::scanner::parse_frontmatter;

use super::{operational_read_audit, AppState};

const REPO_CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes

struct RepoEntry {
    dir: PathBuf,
    expires: Instant,
}

static REPO_CACHE: Mutex<Option<HashMap<String, RepoEntry>>> = Mutex::new(None);

fn skills_dir() -> PathBuf {
    sentinel_infrastructure::paths::home_root_or_fatal()
        .join(".claude")
        .join("skills")
}

fn marketplace_json_path() -> PathBuf {
    sentinel_infrastructure::paths::home_root_or_fatal()
        .join(".claude")
        .join("marketplace.json")
}

/// Validate owner/repo format (alphanumeric, dots, hyphens, underscores only).
fn is_valid_slug(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Clone or return cached repo directory.
fn get_or_clone_repo(owner: &str, repo: &str) -> Result<PathBuf, String> {
    let key = format!("{owner}/{repo}");

    // Check the cache; drop the guard before doing any I/O.
    let cached = {
        let mut cache = REPO_CACHE.lock().unwrap();
        let cache_map = cache.get_or_insert_with(HashMap::new);
        if let Some(entry) = cache_map.get(&key) {
            if Instant::now() < entry.expires && entry.dir.exists() {
                return Ok(entry.dir.clone());
            }
            // Expired — clean up the old dir (best-effort, ignore errors).
            let _ = fs::remove_dir_all(&entry.dir);
            cache_map.remove(&key);
        }
        None::<PathBuf>
    };
    let _ = cached; // consumed above

    // **Attack #156 fix**: Use a unique temp dir name with a random suffix to
    // prevent symlink race attacks. The previous predictable name
    // `skills-{owner}-{repo}` allowed an attacker to pre-create a symlink at
    // that path, redirecting git clone output to an arbitrary directory.
    let mut rand_bytes = [0u8; 8];
    let _ = getrandom::getrandom(&mut rand_bytes);
    // fold avoids a temporary String per byte (vs format_collect pattern).
    let rand_suffix: String = rand_bytes
        .iter()
        .fold(String::with_capacity(16), |mut s, b| {
            use std::fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        });
    let tmp_dir = std::env::temp_dir().join(format!("skills-{owner}-{repo}-{rand_suffix}"));

    // Verify the path doesn't already exist (extremely unlikely with random suffix)
    if tmp_dir.exists() {
        return Err(format!("Temp directory collision: {}", tmp_dir.display()));
    }

    let output = Command::new("git")
        .args([
            "clone",
            "--depth",
            "1",
            &format!("https://github.com/{owner}/{repo}.git"),
            &tmp_dir.to_string_lossy(),
        ])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("Failed to run git: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git clone failed: {stderr}"));
    }

    // Re-acquire guard to insert the newly cloned dir.
    REPO_CACHE
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(
            key,
            RepoEntry {
                dir: tmp_dir.clone(),
                expires: Instant::now() + REPO_CACHE_TTL,
            },
        );

    Ok(tmp_dir)
}

#[derive(Debug, Serialize)]
struct DiscoveredSkill {
    name: String,
    description: String,
    dir_name: String,
    content: String,
}

/// Walk a directory looking for SKILL.md files (max 5 levels deep).
fn discover_skills(dir: &Path) -> Result<Vec<DiscoveredSkill>, String> {
    let mut skills = Vec::new();
    walk_for_skills(dir, 0, 5, &mut skills)?;
    Ok(skills)
}

fn walk_for_skills(
    dir: &Path,
    depth: usize,
    max_depth: usize,
    skills: &mut Vec<DiscoveredSkill>,
) -> Result<(), String> {
    if depth > max_depth {
        return Ok(());
    }
    let entries =
        fs::read_dir(dir).map_err(|e| format!("failed to read dir {}: {e}", dir.display()))?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("failed to read entry in {}: {e}", dir.display()))?;
        let file_type = entry.file_type().map_err(|e| {
            format!(
                "failed to read file type for {}: {e}",
                entry.path().display()
            )
        })?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }

        let sub_dir = entry.path();
        let skill_md = sub_dir.join("SKILL.md");

        if skill_md
            .try_exists()
            .map_err(|e| format!("failed to inspect {}: {e}", skill_md.display()))?
        {
            let content = fs::read_to_string(&skill_md)
                .map_err(|e| format!("failed to read {}: {e}", skill_md.display()))?;
            let fm = parse_frontmatter(&content);
            let skill_name = fm.get("name").ok_or_else(|| {
                format!(
                    "{} missing required frontmatter field 'name'",
                    skill_md.display()
                )
            })?;
            let description = fm.get("description").ok_or_else(|| {
                format!(
                    "{} missing required frontmatter field 'description'",
                    skill_md.display()
                )
            })?;
            skills.push(DiscoveredSkill {
                name: skill_name.clone(),
                description: description.clone(),
                dir_name: name,
                content,
            });
        } else {
            walk_for_skills(&sub_dir, depth + 1, max_depth, skills)?;
        }
    }
    Ok(())
}

fn is_skill_installed(skill_name: &str) -> bool {
    skills_dir().join(skill_name).join("SKILL.md").exists()
}

// --- Handlers ---

#[derive(Serialize)]
struct BrowseSkillEntry {
    name: String,
    dir_name: String,
    description: String,
    installed: bool,
    content_preview: String,
}

#[derive(Serialize)]
struct BrowseResponse {
    owner: String,
    repo: String,
    skills: Vec<BrowseSkillEntry>,
}

async fn browse(
    AxumPath((owner, repo)): AxumPath<(String, String)>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !is_valid_slug(&owner) || !is_valid_slug(&repo) {
        return operational_json(
            OperationalApiReadSurface::StoreBrowse,
            serde_json::json!({"error": "Invalid owner/repo format"}),
        )
        .await;
    }

    let response = match get_or_clone_repo(&owner, &repo) {
        Ok(repo_dir) => match discover_skills(&repo_dir) {
            Ok(skills) => {
                let result: Vec<BrowseSkillEntry> = skills
                    .iter()
                    .map(|s| BrowseSkillEntry {
                        name: s.name.clone(),
                        dir_name: s.dir_name.clone(),
                        description: s.description.clone(),
                        installed: is_skill_installed(&s.dir_name),
                        content_preview: s.content.chars().take(500).collect(),
                    })
                    .collect();

                serde_json::to_value(BrowseResponse {
                    owner,
                    repo,
                    skills: result,
                })
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            }
            Err(e) => serde_json::json!({"error": e}),
        },
        Err(e) => serde_json::json!({"error": e}),
    };
    operational_json(OperationalApiReadSurface::StoreBrowse, response).await
}

#[derive(Serialize)]
struct PreviewResponse {
    name: String,
    dir_name: String,
    description: String,
    content: String,
}

async fn preview(
    AxumPath((owner, repo, skill)): AxumPath<(String, String, String)>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !is_valid_slug(&owner) || !is_valid_slug(&repo) {
        return operational_json(
            OperationalApiReadSurface::StorePreview,
            serde_json::json!({"error": "Invalid owner/repo format"}),
        )
        .await;
    }

    let response = match get_or_clone_repo(&owner, &repo) {
        Ok(repo_dir) => match discover_skills(&repo_dir) {
            Ok(skills) => {
                match skills
                    .iter()
                    .find(|s| s.dir_name == skill || s.name == skill)
                {
                    Some(found) => serde_json::to_value(PreviewResponse {
                        name: found.name.clone(),
                        dir_name: found.dir_name.clone(),
                        description: found.description.clone(),
                        content: found.content.clone(),
                    })
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
                    None => {
                        serde_json::json!({"error": format!("Skill \"{skill}\" not found in {owner}/{repo}")})
                    }
                }
            }
            Err(e) => serde_json::json!({"error": e}),
        },
        Err(e) => serde_json::json!({"error": e}),
    };
    operational_json(OperationalApiReadSurface::StorePreview, response).await
}

#[derive(Deserialize)]
struct InstallRequest {
    owner: String,
    repo: String,
    skill: String,
}

async fn install(Json(body): Json<InstallRequest>) -> Result<Json<serde_json::Value>, StatusCode> {
    let input = serde_json::json!({
        "owner": &body.owner,
        "repo": &body.repo,
        "skill": &body.skill,
    });
    if !is_valid_slug(&body.owner) || !is_valid_slug(&body.repo) {
        return operational_tool_json(
            "store_install_skill",
            &input,
            serde_json::json!({"error": "Invalid owner/repo format"}),
        )
        .await;
    }

    let repo_dir = match get_or_clone_repo(&body.owner, &body.repo) {
        Ok(repo_dir) => repo_dir,
        Err(e) => {
            return operational_tool_json(
                "store_install_skill",
                &input,
                serde_json::json!({"error": e}),
            )
            .await;
        }
    };

    let skills = match discover_skills(&repo_dir) {
        Ok(skills) => skills,
        Err(e) => {
            return operational_tool_json(
                "store_install_skill",
                &input,
                serde_json::json!({"error": e}),
            )
            .await;
        }
    };
    let Some(found) = skills
        .iter()
        .find(|s| s.dir_name == body.skill || s.name == body.skill)
    else {
        return operational_tool_json(
            "store_install_skill",
            &input,
            serde_json::json!({"error": format!("Skill \"{}\" not found", body.skill)}),
        )
        .await;
    };

    let dest_dir = skills_dir().join(&found.dir_name);
    if dest_dir.exists() {
        return operational_tool_json(
            "store_install_skill",
            &input,
            serde_json::json!({"error": format!("Skill \"{}\" already exists in marketplace", found.dir_name)}),
        )
        .await;
    }

    // Copy skill directory
    let source_dir = repo_dir.join(&found.dir_name);
    if let Err(e) = copy_dir_recursive(&source_dir, &dest_dir) {
        return operational_tool_json(
            "store_install_skill",
            &input,
            serde_json::json!({"error": format!("Failed to copy skill: {e}")}),
        )
        .await;
    }

    // Remove .git from copied skill
    let git_dir = dest_dir.join(".git");
    if git_dir.exists() {
        if let Err(e) = fs::remove_dir_all(&git_dir) {
            return operational_tool_json(
                "store_install_skill",
                &input,
                serde_json::json!({"error": format!("Failed to remove copied .git directory: {e}")}),
            )
            .await;
        }
    }

    // Add to marketplace.json
    match fs::read_to_string(marketplace_json_path())
        .map_err(|e| format!("Failed to read marketplace.json: {e}"))
        .and_then(|content| {
            serde_json::from_str::<serde_json::Value>(&content)
                .map_err(|e| format!("Failed to parse marketplace.json: {e}"))
        }) {
        Ok(mut manifest) => {
            let Some(skills_arr) = manifest.get_mut("skills").and_then(|v| v.as_array_mut()) else {
                return operational_tool_json(
                    "store_install_skill",
                    &input,
                    serde_json::json!({"error": "marketplace.json missing skills array"}),
                )
                .await;
            };
            let already = skills_arr
                .iter()
                .any(|s| s.get("name").and_then(|v| v.as_str()) == Some(&found.dir_name));
            if !already {
                skills_arr.push(serde_json::json!({
                    "name": found.dir_name,
                    "priority": 50,
                    "source": format!("{}/{}", body.owner, body.repo),
                }));
                let serialized = match serde_json::to_string_pretty(&manifest) {
                    Ok(serialized) => serialized + "\n",
                    Err(e) => {
                        return operational_tool_json(
                            "store_install_skill",
                            &input,
                            serde_json::json!({"error": format!("Failed to serialize marketplace.json: {e}")}),
                        )
                        .await;
                    }
                };
                if let Err(e) = fs::write(marketplace_json_path(), serialized) {
                    return operational_tool_json(
                        "store_install_skill",
                        &input,
                        serde_json::json!({"error": format!("Failed to write marketplace.json: {e}")}),
                    )
                    .await;
                }
            }
        }
        Err(e) => {
            return operational_tool_json(
                "store_install_skill",
                &input,
                serde_json::json!({"error": e}),
            )
            .await;
        }
    }

    operational_tool_json(
        "store_install_skill",
        &input,
        serde_json::json!({
        "message": format!("Installed \"{}\" from {}/{}", found.name, body.owner, body.repo),
        "skill": found.dir_name,
        "source": format!("{}/{}", body.owner, body.repo),
        }),
    )
    .await
}

async fn uninstall(
    AxumPath(name): AxumPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let input = serde_json::json!({
        "name": &name,
    });
    let dest_dir = skills_dir().join(&name);
    if !dest_dir.exists() {
        return operational_tool_json(
            "store_uninstall_skill",
            &input,
            serde_json::json!({"error": format!("Skill \"{name}\" not found in marketplace")}),
        )
        .await;
    }

    if let Err(e) = fs::remove_dir_all(&dest_dir) {
        return operational_tool_json(
            "store_uninstall_skill",
            &input,
            serde_json::json!({"error": format!("Failed to remove skill: {e}")}),
        )
        .await;
    }

    // Remove from marketplace.json
    match fs::read_to_string(marketplace_json_path())
        .map_err(|e| format!("Failed to read marketplace.json: {e}"))
        .and_then(|content| {
            serde_json::from_str::<serde_json::Value>(&content)
                .map_err(|e| format!("Failed to parse marketplace.json: {e}"))
        }) {
        Ok(mut manifest) => {
            let Some(skills_arr) = manifest.get_mut("skills").and_then(|v| v.as_array_mut()) else {
                return operational_tool_json(
                    "store_uninstall_skill",
                    &input,
                    serde_json::json!({"error": "marketplace.json missing skills array"}),
                )
                .await;
            };
            skills_arr.retain(|s| s.get("name").and_then(|v| v.as_str()) != Some(&name));
            let serialized = match serde_json::to_string_pretty(&manifest) {
                Ok(serialized) => serialized + "\n",
                Err(e) => {
                    return operational_tool_json(
                        "store_uninstall_skill",
                        &input,
                        serde_json::json!({"error": format!("Failed to serialize marketplace.json: {e}")}),
                    )
                    .await;
                }
            };
            if let Err(e) = fs::write(marketplace_json_path(), serialized) {
                return operational_tool_json(
                    "store_uninstall_skill",
                    &input,
                    serde_json::json!({"error": format!("Failed to write marketplace.json: {e}")}),
                )
                .await;
            }
        }
        Err(e) => {
            return operational_tool_json(
                "store_uninstall_skill",
                &input,
                serde_json::json!({"error": e}),
            )
            .await;
        }
    }

    operational_tool_json(
        "store_uninstall_skill",
        &input,
        serde_json::json!({"message": format!("Removed \"{name}\" from marketplace")}),
    )
    .await
}

async fn operational_json(
    surface: OperationalApiReadSurface,
    response: serde_json::Value,
) -> Result<Json<serde_json::Value>, StatusCode> {
    operational_read_audit::attach_operational_api_read_graph_audit(surface, response)
        .await
        .map(Json)
        .map_err(|error| {
            tracing::error!(
                surface = sentinel_infrastructure::operational_api_read_graph::operational_api_read_surface_label(surface),
                error = %error,
                "store API read graph audit failed; refusing unaudited response"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

async fn operational_tool_json(
    operation: &str,
    input: &serde_json::Value,
    response: serde_json::Value,
) -> Result<Json<serde_json::Value>, StatusCode> {
    match crate::claude_md_cmd::run_operational_tool_graph_audit(operation, input, &response).await
    {
        Ok(graph_audit) => Ok(Json(
            crate::claude_md_cmd::attach_operational_tool_graph_audit(response, graph_audit),
        )),
        Err(error) => {
            tracing::error!(
                operation,
                error = %error,
                "store API mutation graph audit failed; refusing unaudited response"
            );
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Recursively copy a directory.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/store/browse/{owner}/{repo}", get(browse))
        .route("/store/preview/{owner}/{repo}/{skill}", get(preview))
        .route("/store/install", post(install))
        .route("/store/uninstall/{name}", delete(uninstall))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        previous_home: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set_sentinel_home(path: &std::path::Path) -> Self {
            let previous_home = std::env::var_os("SENTINEL_HOME");
            std::env::set_var("SENTINEL_HOME", path);
            Self { previous_home }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous_home.take() {
                Some(value) => std::env::set_var("SENTINEL_HOME", value),
                None => std::env::remove_var("SENTINEL_HOME"),
            }
        }
    }

    #[test]
    fn store_paths_use_authoritative_home_root() {
        let _guard = crate::test_env::lock();
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let _env = EnvGuard::set_sentinel_home(tmp.path());

        assert_eq!(skills_dir(), tmp.path().join(".claude").join("skills"));
        assert_eq!(
            marketplace_json_path(),
            tmp.path().join(".claude").join("marketplace.json")
        );
    }

    #[test]
    fn discover_skills_reads_valid_skill() {
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let skill_dir = tmp.path().join("valid-skill");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: valid-skill\ndescription: Valid skill\n---\n# Valid\n",
        )
        .expect("skill file");

        let skills = discover_skills(tmp.path()).expect("valid skill discovery");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "valid-skill");
        assert_eq!(skills[0].description, "Valid skill");
    }

    #[test]
    fn discover_skills_errors_on_skill_without_name_frontmatter() {
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let skill_dir = tmp.path().join("broken-skill");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(skill_dir.join("SKILL.md"), "# Broken\n").expect("skill file");

        let err = discover_skills(tmp.path()).expect_err("missing name must fail closed");
        assert!(err.contains("SKILL.md"));
        assert!(err.contains("missing required frontmatter field 'name'"));
    }

    #[test]
    fn discover_skills_errors_on_skill_without_description_frontmatter() {
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let skill_dir = tmp.path().join("broken-skill");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: broken-skill\n---\n# Broken\n",
        )
        .expect("skill file");

        let err = discover_skills(tmp.path()).expect_err("missing description must fail closed");
        assert!(err.contains("SKILL.md"));
        assert!(err.contains("missing required frontmatter field 'description'"));
    }
}
