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
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use sentinel_application::scanner::parse_frontmatter;

use super::AppState;

const REPO_CACHE_TTL: Duration = Duration::from_secs(300); // 5 minutes

struct RepoEntry {
    dir: PathBuf,
    expires: Instant,
}

static REPO_CACHE: Mutex<Option<HashMap<String, RepoEntry>>> = Mutex::new(None);

fn skills_dir() -> PathBuf {
    dirs::home_dir()
        .expect("[sentinel] FATAL: Cannot determine home directory")
        .join(".claude")
        .join("skills")
}

fn marketplace_json_path() -> PathBuf {
    dirs::home_dir()
        .expect("[sentinel] FATAL: Cannot determine home directory")
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
    let rand_suffix: String = rand_bytes.iter().fold(String::with_capacity(16), |mut s, b| {
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

#[derive(Serialize)]
struct DiscoveredSkill {
    name: String,
    description: String,
    dir_name: String,
    content: String,
}

/// Walk a directory looking for SKILL.md files (max 5 levels deep).
fn discover_skills(dir: &Path) -> Vec<DiscoveredSkill> {
    let mut skills = Vec::new();
    walk_for_skills(dir, 0, 5, &mut skills);
    skills
}

fn walk_for_skills(dir: &Path, depth: usize, max_depth: usize, skills: &mut Vec<DiscoveredSkill>) {
    if depth > max_depth {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.filter_map(std::result::Result::ok) {
        if !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }

        let sub_dir = entry.path();
        let skill_md = sub_dir.join("SKILL.md");

        if skill_md.exists() {
            if let Ok(content) = fs::read_to_string(&skill_md) {
                let fm = parse_frontmatter(&content);
                if let Some(skill_name) = fm.get("name") {
                    skills.push(DiscoveredSkill {
                        name: skill_name.clone(),
                        description: fm.get("description").cloned().unwrap_or_default(),
                        dir_name: name,
                        content,
                    });
                }
            }
        } else {
            walk_for_skills(&sub_dir, depth + 1, max_depth, skills);
        }
    }
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

async fn browse(AxumPath((owner, repo)): AxumPath<(String, String)>) -> Json<serde_json::Value> {
    if !is_valid_slug(&owner) || !is_valid_slug(&repo) {
        return Json(serde_json::json!({"error": "Invalid owner/repo format"}));
    }

    match get_or_clone_repo(&owner, &repo) {
        Ok(repo_dir) => {
            let skills = discover_skills(&repo_dir);
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

            Json(
                serde_json::to_value(BrowseResponse {
                    owner,
                    repo,
                    skills: result,
                })
                .unwrap_or_default(),
            )
        }
        Err(e) => Json(serde_json::json!({"error": e})),
    }
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
) -> Json<serde_json::Value> {
    if !is_valid_slug(&owner) || !is_valid_slug(&repo) {
        return Json(serde_json::json!({"error": "Invalid owner/repo format"}));
    }

    match get_or_clone_repo(&owner, &repo) {
        Ok(repo_dir) => {
            let skills = discover_skills(&repo_dir);
            match skills
                .iter()
                .find(|s| s.dir_name == skill || s.name == skill)
            {
                Some(found) => Json(
                    serde_json::to_value(PreviewResponse {
                        name: found.name.clone(),
                        dir_name: found.dir_name.clone(),
                        description: found.description.clone(),
                        content: found.content.clone(),
                    })
                    .unwrap_or_default(),
                ),
                None => Json(
                    serde_json::json!({"error": format!("Skill \"{skill}\" not found in {owner}/{repo}")}),
                ),
            }
        }
        Err(e) => Json(serde_json::json!({"error": e})),
    }
}

#[derive(Deserialize)]
struct InstallRequest {
    owner: String,
    repo: String,
    skill: String,
}

async fn install(Json(body): Json<InstallRequest>) -> Json<serde_json::Value> {
    if !is_valid_slug(&body.owner) || !is_valid_slug(&body.repo) {
        return Json(serde_json::json!({"error": "Invalid owner/repo format"}));
    }

    let Ok(repo_dir) = get_or_clone_repo(&body.owner, &body.repo) else {
        return Json(serde_json::json!({"error": "Failed to clone repo"}));
    };

    let skills = discover_skills(&repo_dir);
    let Some(found) = skills
        .iter()
        .find(|s| s.dir_name == body.skill || s.name == body.skill)
    else {
        return Json(
            serde_json::json!({"error": format!("Skill \"{}\" not found", body.skill)}),
        );
    };

    let dest_dir = skills_dir().join(&found.dir_name);
    if dest_dir.exists() {
        return Json(
            serde_json::json!({"error": format!("Skill \"{}\" already exists in marketplace", found.dir_name)}),
        );
    }

    // Copy skill directory
    let source_dir = repo_dir.join(&found.dir_name);
    if let Err(e) = copy_dir_recursive(&source_dir, &dest_dir) {
        return Json(serde_json::json!({"error": format!("Failed to copy skill: {e}")}));
    }

    // Remove .git from copied skill
    let git_dir = dest_dir.join(".git");
    if git_dir.exists() {
        let _ = fs::remove_dir_all(&git_dir);
    }

    // Add to marketplace.json
    if let Ok(content) = fs::read_to_string(marketplace_json_path()) {
        if let Ok(mut manifest) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(skills_arr) = manifest.get_mut("skills").and_then(|v| v.as_array_mut()) {
                let already = skills_arr
                    .iter()
                    .any(|s| s.get("name").and_then(|v| v.as_str()) == Some(&found.dir_name));
                if !already {
                    skills_arr.push(serde_json::json!({
                        "name": found.dir_name,
                        "priority": 50,
                        "source": format!("{}/{}", body.owner, body.repo),
                    }));
                    let _ = fs::write(
                        marketplace_json_path(),
                        serde_json::to_string_pretty(&manifest).unwrap_or_default() + "\n",
                    );
                }
            }
        }
    }

    Json(serde_json::json!({
        "message": format!("Installed \"{}\" from {}/{}", found.name, body.owner, body.repo),
        "skill": found.dir_name,
        "source": format!("{}/{}", body.owner, body.repo),
    }))
}

async fn uninstall(AxumPath(name): AxumPath<String>) -> Json<serde_json::Value> {
    let dest_dir = skills_dir().join(&name);
    if !dest_dir.exists() {
        return Json(
            serde_json::json!({"error": format!("Skill \"{name}\" not found in marketplace")}),
        );
    }

    if let Err(e) = fs::remove_dir_all(&dest_dir) {
        return Json(serde_json::json!({"error": format!("Failed to remove skill: {e}")}));
    }

    // Remove from marketplace.json
    if let Ok(content) = fs::read_to_string(marketplace_json_path()) {
        if let Ok(mut manifest) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(skills_arr) = manifest.get_mut("skills").and_then(|v| v.as_array_mut()) {
                skills_arr.retain(|s| s.get("name").and_then(|v| v.as_str()) != Some(&name));
                let _ = fs::write(
                    marketplace_json_path(),
                    serde_json::to_string_pretty(&manifest).unwrap_or_default() + "\n",
                );
            }
        }
    }

    Json(serde_json::json!({"message": format!("Removed \"{name}\" from marketplace")}))
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
