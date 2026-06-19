//! Config command — manage ~/.claude/sentinel/user.toml

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use toml::{Table, Value};

fn config_path() -> PathBuf {
    sentinel_infrastructure::paths::sentinel_root().join("user.toml")
}

pub fn set(key: &str, value: &str) -> Result<()> {
    let path = config_path();

    // Load existing config or start fresh
    let mut doc: Table = if path.exists() {
        let content = fs::read_to_string(&path)?;
        content.parse().unwrap_or_default()
    } else {
        Table::new()
    };

    if key == "name" {
        doc.insert(key.to_string(), Value::String(value.to_string()));
    } else {
        eprintln!("Unknown config key: {key}");
        eprintln!("Available keys: name");
        return Ok(());
    }

    // Ensure parent dir exists
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let header = "# Sentinel user configuration\n\n";
    fs::write(&path, format!("{header}{doc}"))?;
    println!("Set {key} = \"{value}\" in {}", path.display());
    Ok(())
}

pub fn show() -> Result<()> {
    let path = config_path();
    if !path.exists() {
        println!("No user config found at {}", path.display());
        println!("Run: sentinel config set name \"Your Name\"");
        return Ok(());
    }

    let content = fs::read_to_string(&path)?;
    println!("Config: {}\n", path.display());
    print!("{content}");
    Ok(())
}
