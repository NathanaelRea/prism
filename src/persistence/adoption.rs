use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use sqlx::{Connection, SqliteConnection};

use super::error::DatabaseError;
use super::pools::{close_connection, options, set_owner_only, validate_integrity};

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

const RELEASED_MIGRATION_1_CHECKSUM: &str = "AFA7799DCF872B465A2F56538B06E5AF6023B8B55256C1BC3A4136A046047F6B73AAD5EE40554F922C62D19CF40EEC4C";
const RELEASED_MIGRATION_2_CHECKSUM: &str = "E0A2ACB7A8032D00C9869FEAA578021DBCF1CF865568BFFA97FB82FCDB84237EC19B63895D6C589E5585D6839ECFEBC1";
const CURRENT_MIGRATION_COUNT: usize = 2;

#[derive(Clone, Copy)]
enum Adoption {
    None,
    Historical,
    RebaselineReleasedSqlx { applied: usize },
}

pub(super) async fn adopt_historical_repository_database(
    path: &Path,
    migrator: &sqlx::migrate::Migrator,
    writer_busy_timeout: std::time::Duration,
) -> Result<(), DatabaseError> {
    if !path.exists()
        || std::fs::metadata(path)
            .map(|metadata| metadata.len() == 0)
            .unwrap_or(false)
    {
        return Ok(());
    }

    let mut connection = SqliteConnection::connect_with(&options(path, false, true)?)
        .await
        .map_err(|source| DatabaseError::Connect {
            path: path.into(),
            source,
        })?;
    let inspection = async {
        if migration_table_exists(&mut connection).await? {
            return Ok(match released_sqlx_history(&mut connection).await? {
                Some(applied) => Adoption::RebaselineReleasedSqlx { applied },
                None => Adoption::None,
            });
        }
        classify_released_v2(&mut connection, path).await?;
        validate_integrity(&mut connection).await?;

        Ok(Adoption::Historical)
    }
    .await;
    let adoption = close_connection(connection, inspection).await?;
    if matches!(adoption, Adoption::None) {
        return Ok(());
    }

    if let Adoption::RebaselineReleasedSqlx { applied } = adoption {
        validate_released_sqlx_schema(path, applied).await?;
    }

    let mut connection = SqliteConnection::connect_with(
        &super::pools::options_with_writer_busy_timeout(path, false, false, writer_busy_timeout)?,
    )
    .await
    .map_err(|source| DatabaseError::Connect {
        path: path.into(),
        source,
    })?;
    sqlx::query("pragma foreign_keys = off")
        .execute(&mut connection)
        .await
        .map_err(DatabaseError::Query)?;
    sqlx::query("pragma legacy_alter_table = on")
        .execute(&mut connection)
        .await
        .map_err(DatabaseError::Query)?;
    let backup_extension = match adoption {
        Adoption::Historical => "db.pre-sqlx-backup",
        Adoption::RebaselineReleasedSqlx { .. } => "db.pre-migration-rebaseline-backup",
        Adoption::None => unreachable!(),
    };
    create_current_backup_and_lock(path, &mut connection, backup_extension).await?;

    let adoption_result = async {
        match adoption {
            Adoption::Historical => {
                classify_released_v2(&mut connection, path).await?;
                validate_integrity(&mut connection).await?;
                sqlx::raw_sql(include_str!(
                    "../../sql/database/adopt_legacy_repository.sql"
                ))
                .execute(&mut connection)
                .await
                .map_err(DatabaseError::Query)?;
            }
            Adoption::RebaselineReleasedSqlx { applied: 1 } => {
                validate_integrity(&mut connection).await?;
                sqlx::raw_sql(include_str!("fixtures/released_repository_0002.sql"))
                    .execute(&mut connection)
                    .await
                    .map_err(DatabaseError::Query)?;
            }
            Adoption::RebaselineReleasedSqlx { applied: 2 } => {
                validate_integrity(&mut connection).await?
            }
            Adoption::RebaselineReleasedSqlx { .. } => unreachable!(),
            Adoption::None => unreachable!(),
        }

        let migrated_contract = schema_contract(&mut connection).await?;
        if !canonical_schema_matches(&mut connection, &migrated_contract).await? {
            return Err(DatabaseError::NonCanonicalRepositorySchema { path: path.into() });
        }

        if matches!(adoption, Adoption::Historical) {
            sqlx::query("create table _sqlx_migrations (version bigint primary key, description text not null, installed_on timestamp not null default current_timestamp, success boolean not null, checksum blob not null, execution_time bigint not null)")
                .execute(&mut connection)
                .await
                .map_err(DatabaseError::Query)?;
        }
        let migrations = migrator.iter().take(CURRENT_MIGRATION_COUNT).collect::<Vec<_>>();
        if migrations.len() != CURRENT_MIGRATION_COUNT {
            return Err(DatabaseError::MissingMigrationBaseline);
        }
        for migration in migrations {
            if matches!(adoption, Adoption::Historical) {
                sqlx::query("insert into _sqlx_migrations (version, description, success, checksum, execution_time) values (?, ?, 1, ?, 0)")
                    .bind(migration.version)
                    .bind(migration.description.as_ref())
                    .bind(migration.checksum.as_ref())
                    .execute(&mut connection)
                    .await
                    .map_err(DatabaseError::Query)?;
            } else {
                let result = sqlx::query(
                    "insert into _sqlx_migrations (version, description, success, checksum, execution_time) values (?, ?, 1, ?, 0) on conflict(version) do update set description = excluded.description, checksum = excluded.checksum where _sqlx_migrations.success",
                )
                .bind(migration.version)
                .bind(migration.description.as_ref())
                .bind(migration.checksum.as_ref())
                .execute(&mut connection)
                .await
                .map_err(DatabaseError::Query)?;
                if result.rows_affected() != 1 {
                    return Err(DatabaseError::MissingMigrationBaseline);
                }
            }
        }
        sqlx::query("pragma user_version = 2147483647")
            .execute(&mut connection)
            .await
            .map_err(DatabaseError::Query)?;
        validate_integrity(&mut connection).await
    }
    .await;

    let adoption = adoption_result;

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

async fn create_current_backup_and_lock(
    path: &Path,
    connection: &mut SqliteConnection,
    backup_extension: &str,
) -> Result<(), DatabaseError> {
    static NEXT_BACKUP: AtomicU64 = AtomicU64::new(1);

    let backup = path.with_extension(backup_extension);
    let temporary = path.with_extension(format!(
        "{backup_extension}.tmp-{}-{}",
        std::process::id(),
        NEXT_BACKUP.fetch_add(1, Ordering::Relaxed)
    ));

    loop {
        match std::fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(DatabaseError::Backup {
                    path: path.into(),
                    backup: temporary.clone(),
                    source,
                });
            }
        }
        let before: i64 = sqlx::query_scalar("pragma data_version")
            .fetch_one(&mut *connection)
            .await
            .map_err(DatabaseError::Query)?;
        sqlx::query("vacuum into ?")
            .bind(temporary.to_string_lossy().into_owned())
            .execute(&mut *connection)
            .await
            .map_err(|source| DatabaseError::BackupQuery {
                path: path.into(),
                backup: temporary.clone(),
                source,
            })?;
        sqlx::query("begin immediate")
            .execute(&mut *connection)
            .await
            .map_err(DatabaseError::Query)?;
        let after: i64 = sqlx::query_scalar("pragma data_version")
            .fetch_one(&mut *connection)
            .await
            .map_err(DatabaseError::Query)?;
        if before == after {
            break;
        }
        sqlx::query("rollback")
            .execute(&mut *connection)
            .await
            .map_err(DatabaseError::Query)?;
    }

    set_owner_only(&temporary)?;
    crate::system::file_persistence::commit_staging(&temporary, &backup).map_err(|source| {
        DatabaseError::Backup {
            path: path.into(),
            backup: backup.clone(),
            source,
        }
    })?;
    Ok(())
}

async fn classify_released_v2(
    connection: &mut SqliteConnection,
    path: &Path,
) -> Result<(), DatabaseError> {
    let user_version: i64 = sqlx::query_scalar("pragma user_version")
        .fetch_one(&mut *connection)
        .await
        .map_err(DatabaseError::Query)?;
    if user_version != 2 {
        return Err(DatabaseError::UnknownHistoricalSchema {
            path: path.into(),
            user_version,
        });
    }
    let candidate_contract = schema_contract(connection).await?;
    let mut released = SqliteConnection::connect("sqlite::memory:")
        .await
        .map_err(DatabaseError::Query)?;
    sqlx::raw_sql(include_str!(
        "../../tests/fixtures/sql/repository-v2-progressive.sql"
    ))
    .execute(&mut released)
    .await
    .map_err(DatabaseError::Query)?;
    let released_contract = schema_contract(&mut released).await?;
    if schemas_semantically_match(
        connection,
        &candidate_contract,
        &mut released,
        &released_contract,
    )
    .await?
    {
        Ok(())
    } else {
        Err(DatabaseError::UnknownHistoricalSchema {
            path: path.into(),
            user_version,
        })
    }
}

async fn migration_table_exists(connection: &mut SqliteConnection) -> Result<bool, DatabaseError> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from sqlite_master where type = 'table' and name = '_sqlx_migrations'",
    )
    .fetch_one(connection)
    .await
    .map_err(DatabaseError::Query)?;
    Ok(count == 1)
}

async fn released_sqlx_history(
    connection: &mut SqliteConnection,
) -> Result<Option<usize>, DatabaseError> {
    let migrations: Vec<(i64, String, i64, String)> = sqlx::query_as(
        "select version, description, success, hex(checksum) from _sqlx_migrations order by version",
    )
    .fetch_all(connection)
    .await
    .map_err(DatabaseError::Query)?;
    let released = [
        (1, "initial".into(), 1, RELEASED_MIGRATION_1_CHECKSUM.into()),
        (
            2,
            "drop legacy workflows".into(),
            1,
            RELEASED_MIGRATION_2_CHECKSUM.into(),
        ),
    ];
    Ok(match migrations.as_slice() {
        migrations if migrations == &released[..1] => Some(1),
        migrations if migrations == released => Some(2),
        _ => None,
    })
}

async fn validate_released_sqlx_schema(path: &Path, applied: usize) -> Result<(), DatabaseError> {
    let mut candidate = SqliteConnection::connect_with(&options(path, false, true)?)
        .await
        .map_err(|source| DatabaseError::Connect {
            path: path.into(),
            source,
        })?;
    let result = async {
        validate_integrity(&mut candidate).await?;
        let candidate_contract = schema_contract(&mut candidate).await?;
        let mut expected = SqliteConnection::connect("sqlite::memory:")
            .await
            .map_err(DatabaseError::Query)?;
        sqlx::raw_sql(include_str!("fixtures/released_repository_0001.sql"))
            .execute(&mut expected)
            .await
            .map_err(DatabaseError::Query)?;
        if applied == 2 {
            sqlx::raw_sql(include_str!("fixtures/released_repository_0002.sql"))
                .execute(&mut expected)
                .await
                .map_err(DatabaseError::Query)?;
        }
        let expected_contract = schema_contract(&mut expected).await?;
        if schemas_semantically_match(
            &mut candidate,
            &candidate_contract,
            &mut expected,
            &expected_contract,
        )
        .await?
        {
            Ok(())
        } else {
            Err(DatabaseError::NonCanonicalRepositorySchema { path: path.into() })
        }
    }
    .await;
    close_connection(candidate, result).await
}

async fn canonical_schema_matches(
    candidate: &mut SqliteConnection,
    candidate_contract: &SchemaContract,
) -> Result<bool, DatabaseError> {
    let mut canonical = SqliteConnection::connect("sqlite::memory:")
        .await
        .map_err(DatabaseError::Query)?;
    sqlx::raw_sql(include_str!("../../migrations/repository/0001_initial.sql"))
        .execute(&mut canonical)
        .await
        .map_err(DatabaseError::Query)?;
    sqlx::raw_sql(include_str!(
        "../../migrations/repository/0002_drop_legacy_workflows.sql"
    ))
    .execute(&mut canonical)
    .await
    .map_err(DatabaseError::Query)?;
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
    let identities = |contract: &SchemaContract| {
        contract
            .iter()
            .map(|(kind, name, table, _)| (kind.clone(), name.clone(), table.clone()))
            .collect::<Vec<_>>()
    };
    if identities(candidate_contract) != identities(expected_contract) {
        return Ok(false);
    }

    for ((_, name, _, candidate_sql), (_, _, _, expected_sql)) in candidate_contract
        .iter()
        .filter(|(kind, _, _, _)| kind == "table")
        .zip(
            expected_contract
                .iter()
                .filter(|(kind, _, _, _)| kind == "table"),
        )
    {
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

    for ((kind, _, _, candidate_sql), (_, _, _, expected_sql)) in
        candidate_contract.iter().zip(expected_contract)
    {
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
        (i64, i64),
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
