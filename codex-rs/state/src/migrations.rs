use std::borrow::Cow;

use sqlx::migrate::Migration;
use sqlx::migrate::Migrator;
use sqlx_sqlite::SqlitePool;

pub(crate) static STATE_MIGRATOR: Migrator = sqlx_macros::migrate!("./migrations");
pub(crate) static LOGS_MIGRATOR: Migrator = sqlx_macros::migrate!("./logs_migrations");
pub(crate) static GOALS_MIGRATOR: Migrator = sqlx_macros::migrate!("./goals_migrations");
pub(crate) static MEMORIES_MIGRATOR: Migrator = sqlx_macros::migrate!("./memory_migrations");
pub(crate) static QUEUE_MIGRATOR: Migrator = sqlx_macros::migrate!("./queue_migrations");
pub(crate) static THREAD_HISTORY_MIGRATOR: Migrator =
    sqlx_macros::migrate!("./thread_history_migrations");

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
    .execute(pool)
    .await?;
    Ok(())
}

const ITEM_LIFECYCLE_THREAD_HISTORY_VERSION: i64 = 7;
const CONTEXT_PAUSE_THREAD_HISTORY_VERSION: i64 = 8;

/// Returns the thread-history migrator for a database migrated by an earlier context-policy
/// release, which recorded the context-pause column as version 7.
///
/// Such a database keeps its history: versions 7 and 8 run swapped, so its recorded version 7
/// validates and item lifecycle timestamps arrive as version 8. The earlier release still opens it.
pub(crate) async fn context_pause_first_thread_history_migrator(
    pool: &SqlitePool,
    migrator: &Migrator,
) -> anyhow::Result<Option<Migrator>> {
    let find = |version| {
        migrator
            .migrations
            .iter()
            .find(|migration| migration.version == version)
    };
    let (Some(item_lifecycle), Some(context_pause)) = (
        find(ITEM_LIFECYCLE_THREAD_HISTORY_VERSION),
        find(CONTEXT_PAUSE_THREAD_HISTORY_VERSION),
    ) else {
        return Ok(None);
    };
    let migrations_table_exists = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations'",
    )
    .fetch_optional(pool)
    .await?
    .is_some();
    if !migrations_table_exists {
        return Ok(None);
    }
    let context_pause_first = sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM _sqlx_migrations WHERE version = ? AND checksum = ?",
    )
    .bind(ITEM_LIFECYCLE_THREAD_HISTORY_VERSION)
    .bind(context_pause.checksum.as_ref())
    .fetch_optional(pool)
    .await?
    .is_some();
    if !context_pause_first {
        return Ok(None);
    }
    let migrations = migrator
        .migrations
        .iter()
        .map(|migration| match migration.version {
            ITEM_LIFECYCLE_THREAD_HISTORY_VERSION => Migration {
                version: ITEM_LIFECYCLE_THREAD_HISTORY_VERSION,
                ..context_pause.clone()
            },
            CONTEXT_PAUSE_THREAD_HISTORY_VERSION => Migration {
                version: CONTEXT_PAUSE_THREAD_HISTORY_VERSION,
                ..item_lifecycle.clone()
            },
            _ => migration.clone(),
        })
        .collect::<Vec<_>>();
    Ok(Some(Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: migrator.ignore_missing,
        locking: migrator.locking,
        no_tx: migrator.no_tx,
        table_name: migrator.table_name.clone(),
        create_schemas: migrator.create_schemas.clone(),
    }))
}

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod tests;
