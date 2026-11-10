use std::borrow::Cow;

use crate::sqlite::SqlitePoolWriteExt;
use crate::sqlite::SqliteWriteQueryExt;
use sqlx::SqlStr;
use sqlx::migrate::Migration;
use sqlx::migrate::MigrationType;
use sqlx::migrate::Migrator;
use sqlx_sqlite::SqlitePool;

pub(crate) static STATE_MIGRATOR: Migrator = sqlx_macros::migrate!("./migrations");
pub(crate) static LOGS_MIGRATOR: Migrator = sqlx_macros::migrate!("./logs_migrations");
pub(crate) static GOALS_MIGRATOR: Migrator = sqlx_macros::migrate!("./goals_migrations");
pub(crate) static MEMORIES_MIGRATOR: Migrator = sqlx_macros::migrate!("./memory_migrations");
pub(crate) static QUEUE_MIGRATOR: Migrator = sqlx_macros::migrate!("./queue_migrations");
pub(crate) static THREAD_HISTORY_MIGRATOR: Migrator =
    sqlx_macros::migrate!("./thread_history_migrations");

// Keep the legacy SQL byte-for-byte so its SQLx checksum identifies both releases.
const CONTEXT_PAUSE_SQL: &str = "ALTER TABLE thread_turns ADD COLUMN context_pause_json TEXT;\n";
const FORK_THREAD_HISTORY_MIGRATIONS: &str = "_codex_fork_thread_history_migrations";

/// Allow an older Codex binary to open a database that has already been
/// migrated by a newer binary running in parallel.
///
/// We intentionally ignore applied migration versions that are newer than the
/// embedded migration set. Known migration versions are still validated by
/// checksum, so this only relaxes the "database is ahead of me" case.
fn runtime_migrator(base: &'static Migrator) -> Migrator {
    Migrator {
        migrations: Cow::Borrowed(base.migrations.as_ref()),
        ignore_missing: true,
        locking: base.locking,
        no_tx: base.no_tx,
        table_name: base.table_name.clone(),
        create_schemas: base.create_schemas.clone(),
    }
}

pub(crate) fn runtime_state_migrator() -> Migrator {
    runtime_migrator(&STATE_MIGRATOR)
}

pub(crate) fn runtime_logs_migrator() -> Migrator {
    runtime_migrator(&LOGS_MIGRATOR)
}

pub(crate) fn runtime_goals_migrator() -> Migrator {
    runtime_migrator(&GOALS_MIGRATOR)
}

pub(crate) fn runtime_memories_migrator() -> Migrator {
    runtime_migrator(&MEMORIES_MIGRATOR)
}

pub(crate) fn runtime_queue_migrator() -> Migrator {
    runtime_migrator(&QUEUE_MIGRATOR)
}

// The paginated history projector will call this when it takes ownership of opening the database.
#[allow(dead_code)]
pub(crate) fn runtime_thread_history_migrator() -> Migrator {
    runtime_migrator(&THREAD_HISTORY_MIGRATOR)
}

pub(crate) async fn repair_legacy_recency_migration_version(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    let Some(recency_migration) = migrator
        .migrations
        .iter()
        .find(|migration| migration.version == 39)
    else {
        return Ok(());
    };
    let migrations_table_exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?
    .is_some();
    if !migrations_table_exists {
        return Ok(());
    }

    let legacy_recency_needs_repair = sqlx::query_scalar::<_, i64>(
        r#"
SELECT 1
FROM _sqlx_migrations
WHERE version = ?
  AND checksum = ?
  AND NOT EXISTS (
      SELECT 1 FROM _sqlx_migrations WHERE version = ?
  )
        "#,
    )
    .bind(38_i64)
    .bind(recency_migration.checksum.as_ref())
    .bind(recency_migration.version)
    .fetch_optional(pool)
    .await?
    .is_some();
    if !legacy_recency_needs_repair {
        return Ok(());
    }

    sqlx::query(
        r#"
UPDATE _sqlx_migrations
SET version = ?, description = ?
WHERE version = ?
  AND checksum = ?
  AND NOT EXISTS (
      SELECT 1 FROM _sqlx_migrations WHERE version = ?
  )
        "#,
    )
    .bind(recency_migration.version)
    .bind(recency_migration.description.as_ref())
    .bind(38_i64)
    .bind(recency_migration.checksum.as_ref())
    .bind(recency_migration.version)
    .execute_with_busy_retry(pool)
    .await?;
    Ok(())
}

/// Vacate the upstream version used by an older context-pause release.
pub(crate) async fn repair_legacy_thread_history_migrations(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<()> {
    let Some(lifecycle) = migrator.migrations.iter().find(|migration| migration.version == 7)
    else {
        return Ok(());
    };
    if !migration_table_exists(pool, "_sqlx_migrations").await? {
        return Ok(());
    }
    let pause = Migration::new(
        1,
        Cow::Borrowed("context pause"),
        MigrationType::Simple,
        SqlStr::from_static(CONTEXT_PAUSE_SQL),
        /*no_tx*/ false,
    );
    let Some(version) = sqlx::query_scalar::<_, i64>(
        "SELECT version FROM _sqlx_migrations \
         WHERE version IN (7, 8) AND checksum = ? AND success",
    )
    .bind(pause.checksum.as_ref())
    .fetch_optional(pool)
    .await?
    else {
        return Ok(());
    };

    let mut transaction = pool.begin_immediate().await?;
    let removed = sqlx::query(
        "DELETE FROM _sqlx_migrations WHERE version = ? AND checksum = ? AND success",
    )
    .bind(version)
    .bind(pause.checksum.as_ref())
    .execute(&mut *transaction)
    .await?;
    if version == 7 && removed.rows_affected() == 1 {
        sqlx::query(
            "UPDATE _sqlx_migrations SET version = 7, description = ? \
             WHERE version = 8 AND checksum = ? AND success",
        )
        .bind(lifecycle.description.as_ref())
        .bind(lifecycle.checksum.as_ref())
        .execute(&mut *transaction)
        .await?;
    }
    transaction.commit().await?;
    Ok(())
}

/// Apply the fork schema after SQLx has applied upstream's migrations.
pub(crate) async fn migrate_fork_thread_history(pool: &SqlitePool) -> anyhow::Result<()> {
    if migration_table_exists(pool, FORK_THREAD_HISTORY_MIGRATIONS).await?
        && sqlx::query_scalar::<_, i64>(
            "SELECT 1 FROM _codex_fork_thread_history_migrations WHERE version = 1",
        )
        .fetch_optional(pool)
        .await?
        .is_some()
    {
        return Ok(());
    }

    let mut transaction = pool.begin_immediate().await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS _codex_fork_thread_history_migrations \
         (version INTEGER PRIMARY KEY)",
    )
    .execute(&mut *transaction)
    .await?;
    let has_pause_column = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM pragma_table_info('thread_turns') WHERE name = 'context_pause_json'",
    )
    .fetch_optional(&mut *transaction)
    .await?
    .is_some();
    if !has_pause_column {
        sqlx::query(CONTEXT_PAUSE_SQL)
            .execute(&mut *transaction)
            .await?;
    }
    sqlx::query("INSERT OR IGNORE INTO _codex_fork_thread_history_migrations (version) VALUES (1)")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(())
}

async fn migration_table_exists(pool: &SqlitePool, name: &str) -> sqlx::Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?
    .is_some())
}

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod tests;
