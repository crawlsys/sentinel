//! Database Operations Gate
//!
//! HARD BLOCK on database migration and destructive commands targeting
//! production environments. The CLAUDE.md says: "NEVER run database ops
//! or migrations in prod or production, even if the user gives permission."
//!
//! Local database ops are allowed (no prod indicators detected).
//! Production is detected by: DATABASE_URL containing "prod", explicit
//! --env production flags, or known production hostnames.

use regex::Regex;
use std::sync::LazyLock;

use sentinel_domain::events::{HookInput, HookOutput};

/// Patterns that indicate a database migration or destructive operation.
static DB_MIGRATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(prisma\s+migrate|diesel\s+migration|sqlx\s+migrate|alembic\s+upgrade|flyway\s+migrate|knex\s+migrate|rake\s+db:migrate|rails\s+db:migrate|sequelize.*migrate|typeorm.*migration:run|drizzle-kit\s+push)"
    ).unwrap()
});

/// Patterns that indicate destructive SQL operations.
static DB_DESTRUCTIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(DROP\s+(TABLE|DATABASE|SCHEMA|INDEX)|TRUNCATE\s+TABLE|DELETE\s+FROM\s+\w+\s*;|ALTER\s+TABLE.*DROP)"
    ).unwrap()
});

/// Patterns that indicate production environment.
static PROD_INDICATOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(--env\s+prod|--environment\s+prod|DATABASE_URL.*prod|\.prod\.|production|--prod\b|-p\s+prod)"
    ).unwrap()
});

/// Process a PreToolUse Bash event. Blocks production database operations.
pub fn process(input: &HookInput) -> HookOutput {
    let cmd = match extract_bash_command(input) {
        Some(c) => c,
        None => return HookOutput::allow(),
    };

    let is_migration = DB_MIGRATION.is_match(cmd);
    let is_destructive = DB_DESTRUCTIVE.is_match(cmd);

    if !is_migration && !is_destructive {
        return HookOutput::allow();
    }

    // Check for production indicators
    if PROD_INDICATOR.is_match(cmd) {
        return HookOutput::deny(
            "🔴 [Database Gate] BLOCKED: Database operation targeting PRODUCTION detected. \
             NEVER run database migrations or destructive operations in production. \
             NO EXCEPTIONS — even if Gary says it's okay."
        );
    }

    // Non-prod migrations/destructive ops: allow but warn
    HookOutput::allow()
}

fn extract_bash_command(input: &HookInput) -> Option<&str> {
    input.tool_input.as_ref()?.get("command")?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bash_input(cmd: &str) -> HookInput {
        HookInput {
            tool_name: Some("Bash".to_string()),
            tool_input: Some(serde_json::json!({"command": cmd})),
            ..Default::default()
        }
    }

    #[test]
    fn test_blocks_prisma_migrate_prod() {
        assert_eq!(
            process(&bash_input("prisma migrate deploy --env production")).blocked,
            Some(true)
        );
    }

    #[test]
    fn test_blocks_diesel_migration_prod() {
        assert_eq!(
            process(&bash_input("DATABASE_URL=postgres://prod.db/app diesel migration run")).blocked,
            Some(true)
        );
    }

    #[test]
    fn test_blocks_drop_table_prod() {
        assert_eq!(
            process(&bash_input("psql production -c 'DROP TABLE users;'")).blocked,
            Some(true)
        );
    }

    #[test]
    fn test_blocks_sqlx_migrate_prod() {
        assert_eq!(
            process(&bash_input("sqlx migrate run --env prod")).blocked,
            Some(true)
        );
    }

    #[test]
    fn test_allows_local_prisma_migrate() {
        assert!(process(&bash_input("prisma migrate dev")).blocked.is_none());
    }

    #[test]
    fn test_allows_local_diesel_migration() {
        assert!(process(&bash_input("diesel migration run")).blocked.is_none());
    }

    #[test]
    fn test_allows_non_db_commands() {
        assert!(process(&bash_input("cargo test")).blocked.is_none());
        assert!(process(&bash_input("git push")).blocked.is_none());
    }

    #[test]
    fn test_allows_no_tool_input() {
        assert!(process(&HookInput::default()).blocked.is_none());
    }

    #[test]
    fn test_blocks_truncate_prod() {
        assert_eq!(
            process(&bash_input("psql production -c 'TRUNCATE TABLE sessions;'")).blocked,
            Some(true)
        );
    }
}
