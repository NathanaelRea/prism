use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, Row, SqliteConnection, TypeInfo, ValueRef};

use super::error::DatabaseError;

static INITIALIZED_DATABASES: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

type TransactionQuery =
    sqlx::query::Query<'static, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'static>>;

pub(crate) fn begin_immediate_query() -> TransactionQuery {
    sqlx::query("begin immediate")
}

pub(crate) fn commit_query() -> TransactionQuery {
    sqlx::query("commit")
}

pub(crate) fn rollback_query() -> TransactionQuery {
    sqlx::query("rollback")
}

pub(crate) fn initialize(path: &Path) -> Result<(), DatabaseError> {
    let key = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let initialized = INITIALIZED_DATABASES.get_or_init(|| Mutex::new(HashSet::new()));
    let mut initialized = initialized.lock().map_err(|_| {
        DatabaseError::Runtime(std::io::Error::other(
            "repository database initialization state is poisoned",
        ))
    })?;
    if initialized.contains(&key) && path.exists() {
        return Ok(());
    }
    crate::async_runtime::block_on(initialize_async(path)).map_err(DatabaseError::Runtime)??;
    initialized.insert(key);
    Ok(())
}

pub(crate) fn open_writable(path: &Path) -> Result<SqliteConnection, DatabaseError> {
    initialize(path)?;
    connect_writable(path)
}

pub(crate) fn connect_writable(path: &Path) -> Result<SqliteConnection, DatabaseError> {
    let options = writable_options(path, false)?;
    connect(path, options)
}

pub(crate) fn open_readonly(path: &Path) -> Result<SqliteConnection, DatabaseError> {
    if !path.exists() {
        return Err(DatabaseError::Connect {
            path: path.to_path_buf(),
            source: sqlx::Error::Configuration(
                format!("database does not exist: {}", path.display()).into(),
            ),
        });
    }
    let options = SqliteConnectOptions::from_str(&path.to_string_lossy())
        .map_err(|source| DatabaseError::Connect {
            path: path.to_path_buf(),
            source,
        })?
        .read_only(true)
        .create_if_missing(false)
        .foreign_keys(true)
        .busy_timeout(Duration::ZERO);
    let mut connection = connect(path, options)?;
    // SQLX_RUNTIME_SQL: SQLite connection policy PRAGMAs are runtime-only statements.
    block_on(sqlx::query("pragma query_only = on").execute(&mut connection))?;
    Ok(connection)
}

fn connect(path: &Path, options: SqliteConnectOptions) -> Result<SqliteConnection, DatabaseError> {
    crate::async_runtime::block_on(SqliteConnection::connect_with(&options))
        .map_err(DatabaseError::Runtime)?
        .map_err(|source| DatabaseError::Connect {
            path: path.to_path_buf(),
            source,
        })
}

pub(crate) fn load_metadata(path: &Path, key: &str) -> Result<Option<String>, DatabaseError> {
    let mut connection = open_writable(path)?;
    block_on(async {
        sqlx::query_file_scalar!("sql/metadata/load.sql", key)
            .fetch_optional(&mut connection)
            .await
    })
}

pub(crate) fn upsert_metadata(path: &Path, key: &str, value: &str) -> Result<(), DatabaseError> {
    let mut connection = open_writable(path)?;
    block_on(async {
        sqlx::query_file!("sql/metadata/upsert.sql", key, value)
            .execute(&mut connection)
            .await?;
        Ok(())
    })
}

pub(crate) fn delete_metadata(path: &Path, key: &str) -> Result<(), DatabaseError> {
    let mut connection = open_writable(path)?;
    block_on(async {
        sqlx::query_file!("sql/metadata/delete.sql", key)
            .execute(&mut connection)
            .await?;
        Ok(())
    })
}

pub(crate) fn run_operator_query(path: &Path, query: &str) -> Result<Vec<Vec<String>>, String> {
    let mut connection = open_readonly(path).map_err(|error| error.to_string())?;
    block_on(async {
        // SQLX_RUNTIME_SQL: `prism db` intentionally executes operator-supplied read-only SQL.
        let rows = sqlx::query(query).fetch_all(&mut connection).await?;
        rows.iter()
            .map(|row| {
                row.columns()
                    .iter()
                    .enumerate()
                    .map(|(index, _)| sqlite_value_to_string(row, index))
                    .collect()
            })
            .collect()
    })
    .map_err(|error| error.to_string())
}

fn sqlite_value_to_string(
    row: &sqlx::sqlite::SqliteRow,
    index: usize,
) -> Result<String, sqlx::Error> {
    let value = row.try_get_raw(index)?;
    if value.is_null() {
        return Ok(String::new());
    }
    match value.type_info().name() {
        "INTEGER" => row.try_get::<i64, _>(index).map(|value| value.to_string()),
        "REAL" => row.try_get::<f64, _>(index).map(|value| value.to_string()),
        "TEXT" => row
            .try_get::<String, _>(index)
            .map(|value| crate::util::single_line(&value)),
        "BLOB" => row
            .try_get::<Vec<u8>, _>(index)
            .map(|value| format!("<blob {} bytes>", value.len())),
        other => Err(sqlx::Error::Decode(
            format!("unsupported SQLite type {other}").into(),
        )),
    }
}

async fn initialize_async(path: &Path) -> Result<(), DatabaseError> {
    super::pools::initialize_repository_database(path).await
}

pub(super) fn writable_options(
    path: &Path,
    create: bool,
) -> Result<SqliteConnectOptions, DatabaseError> {
    super::pools::options(path, create, false)
}

pub(super) fn block_on<T>(
    future: impl std::future::Future<Output = Result<T, sqlx::Error>>,
) -> Result<T, DatabaseError> {
    crate::async_runtime::block_on(future)
        .map_err(DatabaseError::Runtime)?
        .map_err(DatabaseError::Query)
}

#[cfg(test)]
pub(crate) enum TestValue {
    Integer(i64),
    Text(String),
    OptionalInteger(Option<i64>),
}

#[cfg(test)]
impl From<i64> for TestValue {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

#[cfg(test)]
impl From<i32> for TestValue {
    fn from(value: i32) -> Self {
        Self::Integer(i64::from(value))
    }
}

#[cfg(test)]
impl From<u32> for TestValue {
    fn from(value: u32) -> Self {
        Self::Integer(i64::from(value))
    }
}

#[cfg(test)]
impl From<bool> for TestValue {
    fn from(value: bool) -> Self {
        Self::Integer(i64::from(value))
    }
}

#[cfg(test)]
impl From<Option<i64>> for TestValue {
    fn from(value: Option<i64>) -> Self {
        Self::OptionalInteger(value)
    }
}

#[cfg(test)]
impl From<String> for TestValue {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

#[cfg(test)]
impl From<&String> for TestValue {
    fn from(value: &String) -> Self {
        Self::Text(value.clone())
    }
}

#[cfg(test)]
impl From<&str> for TestValue {
    fn from(value: &str) -> Self {
        Self::Text(value.to_string())
    }
}

#[cfg(test)]
impl From<&&str> for TestValue {
    fn from(value: &&str) -> Self {
        Self::Text((*value).to_string())
    }
}

#[cfg(test)]
#[macro_export]
macro_rules! sqlx_test_params {
    ($($value:expr),* $(,)?) => {
        vec![$($crate::persistence::database::TestValue::from($value)),*]
    };
}

#[cfg(test)]
pub(crate) struct TestDatabase {
    path: std::path::PathBuf,
}

#[cfg(test)]
pub(crate) struct TestConnection {
    connection: SqliteConnection,
}

#[cfg(test)]
pub(crate) struct TestRow(sqlx::sqlite::SqliteRow);

#[cfg(test)]
impl TestRow {
    pub(crate) fn get<I, T>(&self, index: I) -> Result<T, sqlx::Error>
    where
        I: sqlx::ColumnIndex<sqlx::sqlite::SqliteRow>,
        T: for<'decode> sqlx::Decode<'decode, sqlx::Sqlite> + sqlx::Type<sqlx::Sqlite>,
    {
        self.0.try_get(index)
    }
}

#[cfg(test)]
impl TestDatabase {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        initialize(path).map_err(|error| error.to_string())?;
        Ok(Self { path: path.into() })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn execute(
        &self,
        statement: &str,
        values: impl AsRef<[TestValue]>,
    ) -> Result<u64, String> {
        let mut connection = connect_writable(&self.path).map_err(|error| error.to_string())?;
        block_on(async {
            // SQLX_RUNTIME_SQL: test-only binding adapter receives fixture SQL from callers.
            let mut query = sqlx::query(statement);
            for value in values.as_ref() {
                query = match value {
                    TestValue::Integer(value) => query.bind(*value),
                    TestValue::Text(value) => query.bind(value),
                    TestValue::OptionalInteger(value) => query.bind(*value),
                };
            }
            query
                .execute(&mut connection)
                .await
                .map(|result| result.rows_affected())
        })
        .map_err(|error| error.to_string())
    }

    pub(crate) fn execute_batch(&self, statements: &str) -> Result<(), String> {
        let mut connection = connect_writable(&self.path).map_err(|error| error.to_string())?;
        block_on(async {
            // SQLX_RUNTIME_SQL: test-only batch adapter receives fixture SQL from callers.
            sqlx::raw_sql(statements).execute(&mut connection).await?;
            Ok(())
        })
        .map_err(|error| error.to_string())
    }

    pub(crate) fn query_row<T>(
        &self,
        statement: &str,
        values: impl AsRef<[TestValue]>,
        map: impl FnOnce(&TestRow) -> Result<T, sqlx::Error>,
    ) -> Result<T, String> {
        let mut connection = connect_writable(&self.path).map_err(|error| error.to_string())?;
        let row = block_on(async {
            // SQLX_RUNTIME_SQL: test-only binding adapter receives fixture SQL from callers.
            let mut query = sqlx::query(statement);
            for value in values.as_ref() {
                query = match value {
                    TestValue::Integer(value) => query.bind(*value),
                    TestValue::Text(value) => query.bind(value),
                    TestValue::OptionalInteger(value) => query.bind(*value),
                };
            }
            query.fetch_one(&mut connection).await
        })
        .map_err(|error| error.to_string())?;
        map(&TestRow(row)).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
impl TestConnection {
    pub(crate) fn open_writable(path: &Path) -> Result<Self, String> {
        super::storage::prepare_writable(path).map_err(|error| error.to_string())?;
        let connection = connect_writable(path).map_err(|error| error.to_string())?;
        Ok(Self { connection })
    }

    pub(crate) fn open_readonly(path: &Path) -> Result<Self, String> {
        let connection = open_readonly(path).map_err(|error| error.to_string())?;
        Ok(Self { connection })
    }

    pub(crate) fn execute_batch(&mut self, statements: &str) -> Result<(), String> {
        block_on(async {
            // SQLX_RUNTIME_SQL: test-only connection fixture receives SQL from test callers.
            sqlx::raw_sql(statements)
                .execute(&mut self.connection)
                .await?;
            Ok(())
        })
        .map_err(|error| error.to_string())
    }

    pub(crate) fn scalar_bool(&mut self, query: &str, value: &str) -> Result<bool, String> {
        block_on(async {
            // SQLX_RUNTIME_SQL: test-only assertion helper receives fixture SQL from callers.
            sqlx::query_scalar::<_, bool>(query)
                .bind(value)
                .fetch_one(&mut self.connection)
                .await
        })
        .map_err(|error| error.to_string())
    }

    pub(crate) fn scalar_i64(&mut self, query: &str) -> Result<i64, String> {
        block_on(async {
            // SQLX_RUNTIME_SQL: test-only assertion helper receives fixture SQL from callers.
            sqlx::query_scalar::<_, i64>(query)
                .fetch_one(&mut self.connection)
                .await
        })
        .map_err(|error| error.to_string())
    }

    pub(crate) fn scalar_string(&mut self, query: &str) -> Result<String, String> {
        block_on(async {
            // SQLX_RUNTIME_SQL: test-only assertion helper receives fixture SQL from callers.
            sqlx::query_scalar::<_, String>(query)
                .fetch_one(&mut self.connection)
                .await
        })
        .map_err(|error| error.to_string())
    }

    pub(crate) fn scalar_string_with(
        &mut self,
        query: &str,
        value: &str,
    ) -> Result<String, String> {
        block_on(async {
            // SQLX_RUNTIME_SQL: test-only assertion helper receives fixture SQL from callers.
            sqlx::query_scalar::<_, String>(query)
                .bind(value)
                .fetch_one(&mut self.connection)
                .await
        })
        .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);

    fn test_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "prism-sqlx-{label}-{}-{}.db",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn remove_database(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn fresh_database_is_migrated_and_reinitialization_is_a_no_op() {
        let path = test_path("fresh");

        initialize(&path).unwrap();
        initialize(&path).unwrap();

        let mut connection = open_readonly(&path).unwrap();
        let migration_count: i64 = block_on(async {
            sqlx::query_file_scalar!("sql/database/test_count_successful_migrations.sql")
                .fetch_one(&mut connection)
                .await
        })
        .unwrap();
        let auto_version_count: i64 = block_on(async {
            sqlx::query_file_scalar!(
                "sql/database/test_count_schema_object.sql",
                "auto_schema_version"
            )
            .fetch_one(&mut connection)
            .await
        })
        .unwrap();
        assert_eq!(migration_count, 1);
        assert_eq!(auto_version_count, 0);
        assert_eq!(
            block_on(async {
                sqlx::query_file_scalar!("sql/database/test_quick_check.sql")
                    .fetch_one(&mut connection)
                    .await
            })
            .unwrap(),
            Some("ok".to_string())
        );
        let foreign_key_violations = block_on(async {
            sqlx::query_file!("sql/database/test_foreign_key_check.sql")
                .fetch_all(&mut connection)
                .await
        })
        .unwrap()
        .len();
        assert_eq!(foreign_key_violations, 0);
        drop(connection);
        remove_database(&path);
    }

    #[test]
    fn unknown_historical_database_is_rejected_without_modification() {
        let path = test_path("unowned");
        let options = writable_options(&path, true).unwrap();
        let mut connection = connect(&path, options).unwrap();
        block_on(async {
            // SQLx cannot check a fixture that intentionally creates a schema outside migrations.
            sqlx::raw_sql(include_str!("../../sql/database/test_create_unowned.sql"))
                .execute(&mut connection)
                .await?;
            // Stabilize the main database bytes before asserting rejected initialization is inert.
            sqlx::query(include_str!("../../sql/database/test_checkpoint.sql"))
                .fetch_one(&mut connection)
                .await?;
            Ok(())
        })
        .unwrap();
        drop(connection);
        let before = std::fs::read(&path).unwrap();

        let error = initialize(&path).unwrap_err();

        assert!(matches!(
            error,
            DatabaseError::UnknownHistoricalSchema { .. }
        ));
        assert!(error.to_string().contains("unknown historical schema"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        remove_database(&path);
    }

    #[test]
    fn historical_database_with_only_anchor_tables_matching_is_rejected() {
        let path = test_path("partial-historical");
        let options = writable_options(&path, true).unwrap();
        let mut connection = connect(&path, options).unwrap();
        block_on(async {
            sqlx::raw_sql(include_str!("../../migrations/repository/0001_initial.sql"))
                .execute(&mut connection)
                .await?;
            sqlx::query("drop table pr_cache")
                .execute(&mut connection)
                .await?;
            sqlx::query("pragma user_version = 1")
                .execute(&mut connection)
                .await?;
            sqlx::query(include_str!("../../sql/database/test_checkpoint.sql"))
                .fetch_one(&mut connection)
                .await?;
            Ok(())
        })
        .unwrap();
        drop(connection);
        let before = std::fs::read(&path).unwrap();

        assert!(matches!(
            initialize(&path),
            Err(DatabaseError::UnknownHistoricalSchema { .. })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!path.with_extension("db.pre-sqlx-backup").exists());
        remove_database(&path);
    }

    #[test]
    fn released_pre_sqlx_database_is_backed_up_and_adopted() {
        let path = test_path("adopt");
        std::fs::copy("tests/fixtures/database/repository-v1.db", &path).unwrap();
        let options = writable_options(&path, false).unwrap();
        let mut connection = connect(&path, options).unwrap();
        block_on(async {
            sqlx::query("insert into metadata (key, value) values ('preserved', 'yes')")
                .execute(&mut connection)
                .await?;
            Ok(())
        })
        .unwrap();
        block_on(connection.close()).unwrap();

        initialize(&path).unwrap();

        assert_eq!(
            load_metadata(&path, "preserved").unwrap().as_deref(),
            Some("yes")
        );
        let backup = path.with_extension("db.pre-sqlx-backup");
        assert!(backup.is_file());
        let mut backup_connection = open_readonly(&backup).unwrap();
        let preserved: String = block_on(async {
            sqlx::query_scalar("select value from metadata where key = 'preserved'")
                .fetch_one(&mut backup_connection)
                .await
        })
        .unwrap();
        assert_eq!(preserved, "yes");
        drop(backup_connection);
        remove_database(&backup);
        remove_database(&path);
    }

    #[test]
    fn metadata_queries_round_trip_and_delete_values() {
        let path = test_path("metadata");

        upsert_metadata(&path, "reconciliation", "first").unwrap();
        upsert_metadata(&path, "reconciliation", "second").unwrap();

        assert_eq!(
            load_metadata(&path, "reconciliation").unwrap().as_deref(),
            Some("second")
        );
        delete_metadata(&path, "reconciliation").unwrap();
        assert_eq!(load_metadata(&path, "reconciliation").unwrap(), None);
        remove_database(&path);
    }
}
