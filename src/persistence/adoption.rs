use std::path::Path;

use sqlx::{Connection, SqliteConnection};

use super::error::DatabaseError;
use super::pools::{close_connection, options, set_owner_only, validate_integrity};

pub(super) const SQLX_OWNED_USER_VERSION: i64 = 2_147_483_647;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LegacyRepositorySchema {
    V1,
    V2,
}

type SchemaContract = Vec<(String, String, String, String)>;
type ColumnContract = (String, String, i64, Option<String>, i64, i64);
type ForeignKeyContract = (
    i64,
    i64,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
);
type IndexContract = (
    String,
    i64,
    String,
    i64,
    Vec<(i64, Option<String>, i64, String, i64)>,
);
type TableOptionsContract = (i64, i64);

pub(super) async fn adopt_historical_repository_database(
    path: &Path,
    migrator: &sqlx::migrate::Migrator,
) -> Result<(), DatabaseError> {
    if !path.exists()
        || std::fs::metadata(path)
            .map(|metadata| metadata.len() == 0)
            .unwrap_or(false)
    {
        return Ok(());
    }

    // Classification is read-only. Unknown, corrupt, and future databases must not gain a WAL,
    // backup, migration journal, or other side effect merely because Prism inspected them.
    let mut connection = SqliteConnection::connect_with(&options(path, false, true)?)
        .await
        .map_err(|source| DatabaseError::Connect {
            path: path.into(),
            source,
        })?;
    let inspection = async {
        if sqlx_migration_table_exists(&mut connection).await? {
            let user_version: i64 = sqlx::query_scalar("pragma user_version")
                .fetch_one(&mut connection)
                .await
                .map_err(DatabaseError::Query)?;
            // The ownership fence predates every supported SQLx repository database. A migration
            // journal without it came from an unreleased development build; reject it before SQLx
            // reports a misleading checksum error or attempts any later migration.
            return if user_version == SQLX_OWNED_USER_VERSION {
                Ok(None)
            } else {
                Err(DatabaseError::NonCanonicalRepositorySchema { path: path.into() })
            };
        }
        let schema = classify_legacy_repository(&mut connection, path).await?;
        validate_integrity(&mut connection).await?;
        refuse_protected_legacy_execution(&mut connection, path).await?;

        let backup = path.with_extension("db.pre-sqlx-backup");
        if !backup.exists() {
            // `VACUUM INTO` uses SQLite's consistent snapshot machinery, including committed WAL
            // pages. A plain file copy can silently omit committed state in WAL mode.
            let backup_name = backup.to_string_lossy().into_owned();
            sqlx::query("vacuum into ?")
                .bind(backup_name)
                .execute(&mut connection)
                .await
                .map_err(|source| DatabaseError::Backup {
                    path: path.into(),
                    backup: backup.clone(),
                    source: std::io::Error::other(source.to_string()),
                })?;
            set_owner_only(&backup)?;
        }
        Ok(Some(schema))
    }
    .await;
    let Some(schema) = close_connection(connection, inspection).await? else {
        return Ok(());
    };

    let mut connection = SqliteConnection::connect_with(&options(path, false, false)?)
        .await
        .map_err(|source| DatabaseError::Connect {
            path: path.into(),
            source,
        })?;
    // Renaming cache tables must not rewrite unrelated foreign-key declarations. Explicit
    // foreign-key validation below remains authoritative for the completed transaction.
    sqlx::query("pragma foreign_keys = off")
        .execute(&mut connection)
        .await
        .map_err(DatabaseError::Query)?;
    sqlx::query("pragma legacy_alter_table = on")
        .execute(&mut connection)
        .await
        .map_err(DatabaseError::Query)?;
    sqlx::query("begin immediate")
        .execute(&mut connection)
        .await
        .map_err(DatabaseError::Query)?;

    let adoption = async {
        // Reclassify under the writer lock so a legacy worker cannot change the schema or enqueue
        // protected work between the read-only inspection and the conversion transaction.
        if classify_legacy_repository(&mut connection, path).await? != schema {
            return Err(unknown_schema(path, &mut connection).await?);
        }
        refuse_protected_legacy_execution(&mut connection, path).await?;
        validate_integrity(&mut connection).await?;

        if schema == LegacyRepositorySchema::V1 {
            sqlx::raw_sql(include_str!(
                "../../migrations/historical/repository_v1_to_v2.sql"
            ))
            .execute(&mut connection)
            .await
            .map_err(DatabaseError::Query)?;
        }
        sqlx::raw_sql(include_str!(
            "../../migrations/historical/repository_cache_to_sqlx.sql"
        ))
        .execute(&mut connection)
        .await
        .map_err(DatabaseError::Query)?;

        validate_integrity(&mut connection).await?;
        let migrated_contract = schema_contract(&mut connection).await?;
        if !canonical_repository_schema_matches(
            &mut connection,
            &migrated_contract,
            migrator,
        )
        .await?
        {
            return Err(DatabaseError::NonCanonicalRepositorySchema { path: path.into() });
        }

        let baseline = migrator
            .iter()
            .next()
            .ok_or(DatabaseError::MissingMigrationBaseline)?;
        sqlx::query("create table _sqlx_migrations (version bigint primary key, description text not null, installed_on timestamp not null default current_timestamp, success boolean not null, checksum blob not null, execution_time bigint not null)")
            .execute(&mut connection)
            .await
            .map_err(DatabaseError::Query)?;
        sqlx::query("insert into _sqlx_migrations (version, description, success, checksum, execution_time) values (?, ?, 1, ?, 0)")
            .bind(baseline.version)
            .bind(baseline.description.as_ref())
            .bind(baseline.checksum.as_ref())
            .execute(&mut connection)
            .await
            .map_err(DatabaseError::Query)?;
        sqlx::query("pragma user_version = 2147483647")
            .execute(&mut connection)
            .await
            .map_err(DatabaseError::Query)?;
        Ok::<_, DatabaseError>(())
    }
    .await;

    let result = match adoption {
        Ok(()) => sqlx::query("commit")
            .execute(&mut connection)
            .await
            .map(|_| ())
            .map_err(DatabaseError::Query),
        Err(error) => {
            let _ = sqlx::query("rollback").execute(&mut connection).await;
            Err(error)
        }
    };
    close_connection(connection, result).await
}

pub(super) async fn validate_canonical_repository_database(
    path: &Path,
    migrator: &sqlx::migrate::Migrator,
) -> Result<(), DatabaseError> {
    let mut connection = SqliteConnection::connect_with(&options(path, false, true)?)
        .await
        .map_err(|source| DatabaseError::Connect {
            path: path.into(),
            source,
        })?;
    let result = async {
        let user_version: i64 = sqlx::query_scalar("pragma user_version")
            .fetch_one(&mut connection)
            .await
            .map_err(DatabaseError::Query)?;
        let contract = schema_contract(&mut connection).await?;
        if user_version != SQLX_OWNED_USER_VERSION
            || !sqlx_migration_table_exists(&mut connection).await?
            || !canonical_repository_schema_matches(&mut connection, &contract, migrator).await?
        {
            return Err(DatabaseError::NonCanonicalRepositorySchema { path: path.into() });
        }
        validate_integrity(&mut connection).await
    }
    .await;
    close_connection(connection, result).await
}

async fn classify_legacy_repository(
    connection: &mut SqliteConnection,
    path: &Path,
) -> Result<LegacyRepositorySchema, DatabaseError> {
    let user_version: i64 = sqlx::query_scalar("pragma user_version")
        .fetch_one(&mut *connection)
        .await
        .map_err(DatabaseError::Query)?;
    let anchors: i64 = sqlx::query_scalar(
        "select count(*) from sqlite_master where type = 'table' and name in ('metadata','plan_run','auto_run','workflow_execution','notification_outbox')",
    )
    .fetch_one(&mut *connection)
    .await
    .map_err(DatabaseError::Query)?;
    let contract = schema_contract(connection).await?;
    let matched = match user_version {
        1 if anchors == 5 => legacy_schema_matches(
            connection,
            &contract,
            include_str!("../../migrations/historical/repository_v1.sql"),
        )
        .await?
        .then_some(LegacyRepositorySchema::V1),
        2 if anchors == 5 => legacy_schema_matches(
            connection,
            &contract,
            include_str!("../../migrations/historical/repository_v2.sql"),
        )
        .await?
        .then_some(LegacyRepositorySchema::V2),
        _ => None,
    };
    matched.ok_or(DatabaseError::UnknownHistoricalSchema {
        path: path.into(),
        user_version,
    })
}

async fn unknown_schema(
    path: &Path,
    connection: &mut SqliteConnection,
) -> Result<DatabaseError, DatabaseError> {
    let user_version = sqlx::query_scalar("pragma user_version")
        .fetch_one(connection)
        .await
        .map_err(DatabaseError::Query)?;
    Ok(DatabaseError::UnknownHistoricalSchema {
        path: path.into(),
        user_version,
    })
}

async fn refuse_protected_legacy_execution(
    connection: &mut SqliteConnection,
    path: &Path,
) -> Result<(), DatabaseError> {
    let protected: i64 = sqlx::query_scalar(
        "select (select count(*) from workflow_execution where dispatch_state in ('queued','claimed','recovery_pending')) + (select count(*) from plan_run where status in ('queued','running','paused')) + (select count(*) from auto_run where status in ('queued','running','paused','waiting'))",
    )
    .fetch_one(connection)
    .await
    .map_err(DatabaseError::Query)?;
    if protected == 0 {
        Ok(())
    } else {
        Err(DatabaseError::ProtectedLegacyExecution {
            path: path.into(),
            count: protected,
        })
    }
}

async fn sqlx_migration_table_exists(
    connection: &mut SqliteConnection,
) -> Result<bool, DatabaseError> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from sqlite_master where type = 'table' and name = '_sqlx_migrations'",
    )
    .fetch_one(connection)
    .await
    .map_err(DatabaseError::Query)?;
    Ok(count == 1)
}

async fn legacy_schema_matches(
    candidate: &mut SqliteConnection,
    candidate_contract: &SchemaContract,
    schema: &str,
) -> Result<bool, DatabaseError> {
    let mut expected = SqliteConnection::connect("sqlite::memory:")
        .await
        .map_err(DatabaseError::Query)?;
    sqlx::raw_sql(schema)
        .execute(&mut expected)
        .await
        .map_err(DatabaseError::Query)?;
    let expected_contract = schema_contract(&mut expected).await?;
    schemas_semantically_match(
        candidate,
        candidate_contract,
        &mut expected,
        &expected_contract,
    )
    .await
}

async fn canonical_repository_schema_matches(
    candidate: &mut SqliteConnection,
    candidate_contract: &SchemaContract,
    migrator: &sqlx::migrate::Migrator,
) -> Result<bool, DatabaseError> {
    let mut canonical = SqliteConnection::connect("sqlite::memory:")
        .await
        .map_err(DatabaseError::Query)?;
    migrator
        .run(&mut canonical)
        .await
        .map_err(DatabaseError::Migrate)?;
    let canonical_contract = schema_contract(&mut canonical).await?;
    schemas_semantically_match(
        candidate,
        candidate_contract,
        &mut canonical,
        &canonical_contract,
    )
    .await
}

async fn schemas_semantically_match(
    candidate: &mut SqliteConnection,
    candidate_contract: &SchemaContract,
    expected: &mut SqliteConnection,
    expected_contract: &SchemaContract,
) -> Result<bool, DatabaseError> {
    // Released databases created tables at different points and then added columns in place.
    // Column order and formatting are not semantic in SQLite, so compare object identity and the
    // structural PRAGMAs instead of sqlite_master construction history.
    let identities = |contract: &SchemaContract| {
        contract
            .iter()
            .map(|(kind, name, table, _)| (kind.clone(), name.clone(), table.clone()))
            .collect::<Vec<_>>()
    };
    if identities(candidate_contract) != identities(expected_contract) {
        return Ok(false);
    }

    let candidate_tables = candidate_contract
        .iter()
        .filter(|(kind, _, _, _)| kind == "table")
        .collect::<Vec<_>>();
    let expected_tables = expected_contract
        .iter()
        .filter(|(kind, _, _, _)| kind == "table")
        .collect::<Vec<_>>();
    for (candidate_table, expected_table) in candidate_tables.iter().zip(&expected_tables) {
        let (_, name, _, candidate_sql) = candidate_table;
        let (_, _, _, expected_sql) = expected_table;
        // CHECK and generated expressions are not fully represented by structural PRAGMAs.
        if (expected_sql.contains("check(")
            || expected_sql.contains("check (")
            || expected_sql.contains(" generated "))
            && candidate_sql != expected_sql
        {
            return Ok(false);
        }
        if table_structure(candidate, name).await? != table_structure(expected, name).await? {
            return Ok(false);
        }
    }

    // Views, triggers, partial indexes, and expression indexes retain semantics that the
    // structural table contract cannot fully reconstruct.
    for (candidate_object, expected_object) in candidate_contract.iter().zip(expected_contract) {
        let (kind, _, _, candidate_sql) = candidate_object;
        let (_, _, _, expected_sql) = expected_object;
        let opaque = kind == "view"
            || kind == "trigger"
            || (kind == "index" && expected_sql.contains(" where "));
        if opaque && candidate_sql != expected_sql {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn table_structure(
    connection: &mut SqliteConnection,
    table: &str,
) -> Result<
    (
        Vec<ColumnContract>,
        Vec<ForeignKeyContract>,
        Vec<IndexContract>,
        TableOptionsContract,
    ),
    DatabaseError,
> {
    let options = sqlx::query_as("select wr, strict from pragma_table_list(?)")
        .bind(table)
        .fetch_one(&mut *connection)
        .await
        .map_err(DatabaseError::Query)?;
    let columns = sqlx::query_as(
        "select name, type, \"notnull\", dflt_value, pk, hidden from pragma_table_xinfo(?) order by name",
    )
    .bind(table)
    .fetch_all(&mut *connection)
    .await
    .map_err(DatabaseError::Query)?;
    let foreign_keys = sqlx::query_as(
        "select id, seq, \"table\", \"from\", \"to\", on_update, on_delete, match from pragma_foreign_key_list(?) order by id, seq",
    )
    .bind(table)
    .fetch_all(&mut *connection)
    .await
    .map_err(DatabaseError::Query)?;
    let indexes: Vec<(i64, String, i64, String, i64)> = sqlx::query_as(
        "select seq, name, \"unique\", origin, partial from pragma_index_list(?) order by name",
    )
    .bind(table)
    .fetch_all(&mut *connection)
    .await
    .map_err(DatabaseError::Query)?;
    let mut index_contracts = Vec::with_capacity(indexes.len());
    for (_, name, unique, origin, partial) in indexes {
        let columns = sqlx::query_as(
            "select seqno, name, desc, coll, key from pragma_index_xinfo(?) order by seqno",
        )
        .bind(&name)
        .fetch_all(&mut *connection)
        .await
        .map_err(DatabaseError::Query)?;
        index_contracts.push((name, unique, origin, partial, columns));
    }
    Ok((columns, foreign_keys, index_contracts, options))
}

async fn schema_contract(
    connection: &mut SqliteConnection,
) -> Result<SchemaContract, DatabaseError> {
    let rows: SchemaContract = sqlx::query_as(
        "select type, name, tbl_name, coalesce(sql, '') from sqlite_master where name not like 'sqlite_%' and name <> '_sqlx_migrations' order by type, name",
    )
    .fetch_all(connection)
    .await
    .map_err(DatabaseError::Query)?;
    Ok(rows
        .into_iter()
        .map(|(kind, name, table, sql)| {
            let normalized = sql
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .to_ascii_lowercase();
            (kind, name, table, normalized)
        })
        .collect())
}
