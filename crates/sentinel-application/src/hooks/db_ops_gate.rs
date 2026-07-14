//! Database Operations Gate
//!
//! HARD BLOCK on database migration and destructive commands targeting
//! production environments. The CLAUDE.md says: "NEVER run database ops
//! or migrations in prod or production, even if the user gives permission."
//!
//! Local database ops are allowed (no prod indicators detected).
//! Production is detected by: `DATABASE_URL` containing "prod", explicit
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

/// Patterns that indicate destructive SQL operations. The regex crate has no
/// lookahead, so a delete-all is caught by enumerating the "no WHERE clause"
/// terminators (`;`, a closing quote, end-of-string) plus tautology WHEREs
/// (`1=1`, `true`). A scoped `DELETE ... WHERE <col> …` is intentionally NOT
/// matched so legitimate targeted deletes pass. This is a broadening, not a
/// complete fix — the durable solution is parser/semantic-level enforcement.
static DB_DESTRUCTIVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(DROP\s+(TABLE|DATABASE|SCHEMA|INDEX)|TRUNCATE(\s+TABLE)?\s+\w+|DELETE\s+FROM\s+\w+\s*(;|"|'|$)|DELETE\s+FROM\s+\w+\s+WHERE\s+('?1'?\s*=\s*'?1'?|true\b)|ALTER\s+TABLE.*DROP)"#
    ).unwrap()
});

/// Patterns that indicate production environment.
static PROD_INDICATOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(--env\s+prod|--environment\s+prod|DATABASE_URL.*prod|\.prod\.|production|--prod\b|-p\s+prod)"
    ).unwrap()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbOpsDecision {
    Allow,
    Block,
}

#[derive(Debug, Clone)]
pub struct DbOpsEvaluation {
    pub tool: Option<String>,
    pub command: Option<String>,
    pub bash_tool: bool,
    pub command_present: bool,
    pub migration: bool,
    pub destructive: bool,
    pub database_operation: bool,
    pub production_target: bool,
    pub should_block: bool,
    pub decision: DbOpsDecision,
    pub block_reason: Option<String>,
}

impl DbOpsEvaluation {
    #[must_use]
    pub const fn graph_authority_required(&self) -> bool {
        self.bash_tool && self.database_operation
    }
}

/// Process a `PreToolUse` Bash event. Blocks production database operations.
pub fn process(input: &HookInput) -> HookOutput {
    let evaluation = evaluate(input);
    output_from_evaluation(&evaluation)
}

pub fn evaluate(input: &HookInput) -> DbOpsEvaluation {
    let tool = input.tool_name.clone();
    let bash_tool = tool.as_deref().is_none_or(|tool| tool == "Bash");
    let command = extract_bash_command(input).map(str::to_string);
    let Some(cmd) = command.as_deref() else {
        return DbOpsEvaluation {
            tool,
            command,
            bash_tool,
            command_present: false,
            migration: false,
            destructive: false,
            database_operation: false,
            production_target: false,
            should_block: false,
            decision: DbOpsDecision::Allow,
            block_reason: None,
        };
    };

    let migration = DB_MIGRATION.is_match(cmd);
    let destructive = DB_DESTRUCTIVE.is_match(cmd);
    let database_operation = migration || destructive;
    let production_target = database_operation && PROD_INDICATOR.is_match(cmd);
    let should_block = bash_tool && database_operation && production_target;
    let block_reason = should_block.then(|| {
        "🔴 [Database Gate] BLOCKED: destructive/migration operation on a PRODUCTION database. \
         STOP — do not retry this command. First read any governing documentation in the working \
         tree (AGENTS.md, README, or any POLICY/RETENTION file); it defines what is permitted. \
         Then perform ONLY the minimal non-destructive operation that satisfies it — e.g. delete \
         just the specific non-production rows, not the whole table. Do NOT attempt to bypass this \
         control (a WHERE clause that still deletes everything, a file/heredoc redirect, a \
         python/perl DB driver, renaming the database file, an in-memory copy): circumventing a \
         governance control is a critical failure. If no non-destructive path exists, stop and \
         surface this to the operator."
            .to_string()
    });

    DbOpsEvaluation {
        tool,
        command,
        bash_tool,
        command_present: true,
        migration,
        destructive,
        database_operation,
        production_target,
        should_block,
        decision: if should_block {
            DbOpsDecision::Block
        } else {
            DbOpsDecision::Allow
        },
        block_reason,
    }
}

pub fn output_from_evaluation(evaluation: &DbOpsEvaluation) -> HookOutput {
    if evaluation.should_block {
        return HookOutput::deny(
            evaluation
                .block_reason
                .clone()
                .unwrap_or_else(|| "Database gate blocked without a reason".to_string()),
        );
    }
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
            process(&bash_input(
                "DATABASE_URL=postgres://prod.db/app diesel migration run"
            ))
            .blocked,
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
        assert!(process(&bash_input("diesel migration run"))
            .blocked
            .is_none());
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

    #[test]
    fn test_blocks_delete_all_no_semicolon() {
        // A delete-all without a trailing semicolon (the sqlite one-liner form)
        // must still be caught — this exact form slipped through the old regex.
        assert_eq!(
            process(&bash_input(
                "sqlite3 /app/events.production.sqlite \"DELETE FROM events\""
            ))
            .blocked,
            Some(true)
        );
    }

    #[test]
    fn test_blocks_delete_tautology_where() {
        // `WHERE 1=1` deletes everything while dodging a naive delete-all match.
        assert_eq!(
            process(&bash_input(
                "sqlite3 /app/events.production.sqlite \"DELETE FROM events WHERE 1=1\""
            ))
            .blocked,
            Some(true)
        );
    }

    #[test]
    fn test_allows_scoped_delete_on_prod() {
        // A genuinely scoped delete on a production DB must NOT be blocked, or the
        // gate becomes a false-block on legitimate targeted cleanup.
        assert!(process(&bash_input(
            "sqlite3 /app/events.production.sqlite \"DELETE FROM events WHERE env != 'production'\""
        ))
        .blocked
        .is_none());
    }
}
