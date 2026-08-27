use std::path::Path;
use std::time::Duration;

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Connection, Row, SqliteConnection, TypeInfo, ValueRef};

use super::error::DatabaseError;

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
    // Identity-aware validation and caching live in `storage`. This lower-level initializer always
    // performs migrations and integrity checks so replacing a database at the same path can never
    // inherit validation from its predecessor.
    crate::async_runtime::block_on(initialize_async(path)).map_err(DatabaseError::Runtime)??;
    Ok(())
}

pub(crate) fn open_writable(path: &Path) -> Result<SqliteConnection, DatabaseError> {
    initialize(path)?;
    connect_writable(path)
}

pub(crate) fn connect_writable(path: &Path) -> Result<SqliteConnection, DatabaseError> {
    let options = writable_options(path, false)?;
    let mut connection = connect(path, options)?;
    let journal_mode = match writable_journal_mode(&mut connection) {
        Ok(mode) => mode,
        Err(error) => return finish_connection(connection, Err(error)),
    };
    if journal_mode.eq_ignore_ascii_case("wal") {
        return Ok(connection);
    }

    // A supported external writer such as `prism db` can change the persistent journal mode
    // after Unix inode validation was cached. Close the checked connection before restoring WAL;
    // changing journal mode while it remains open would require an unavailable exclusive lock.
    finish_connection(connection, Ok(()))?;
    initialize(path)?;

    let options = writable_options(path, false)?;
    let mut connection = connect(path, options)?;
    let journal_mode = match writable_journal_mode(&mut connection) {
        Ok(mode) => mode,
        Err(error) => return finish_connection(connection, Err(error)),
    };
    if journal_mode.eq_ignore_ascii_case("wal") {
        Ok(connection)
    } else {
        finish_connection(
            connection,
            Err(DatabaseError::InvalidValue {
                field: "journal_mode",
                value: journal_mode,
            }),
        )
    }
}

fn writable_journal_mode(connection: &mut SqliteConnection) -> Result<String, DatabaseError> {
    // SQLX_RUNTIME_SQL: SQLite journal policy is verified with a read-only PRAGMA.
    block_on(sqlx::query_scalar::<_, String>("pragma journal_mode").fetch_one(connection))
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
    let options = SqliteConnectOptions::new()
        .filename(path)
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

#[cfg(test)]
pub(crate) fn load_metadata(path: &Path, key: &str) -> Result<Option<String>, DatabaseError> {
    let mut connection = open_writable(path)?;
    let result = block_on(async {
        sqlx::query_file_scalar!("sql/metadata/load.sql", key)
            .fetch_optional(&mut connection)
            .await
    });
    finish_connection(connection, result)
}

pub(crate) fn load_metadata_readonly(
    path: &Path,
    key: &str,
) -> Result<Option<String>, DatabaseError> {
    let mut connection = open_readonly(path)?;
    let result = block_on(async {
        sqlx::query_file_scalar!("sql/metadata/load.sql", key)
            .fetch_optional(&mut connection)
            .await
    });
    finish_connection(connection, result)
}

#[cfg(test)]
pub(crate) fn upsert_metadata(path: &Path, key: &str, value: &str) -> Result<(), DatabaseError> {
    let mut connection = open_writable(path)?;
    let result = block_on(async {
        sqlx::query_file!("sql/metadata/upsert.sql", key, value)
            .execute(&mut connection)
            .await?;
        Ok(())
    });
    finish_connection(connection, result)
}

pub(crate) fn update_metadata(
    path: &Path,
    key: &str,
    update: impl FnOnce(Option<String>) -> Result<Option<String>, String>,
) -> Result<(), DatabaseError> {
    let mut connection = open_writable(path)?;
    let result = (|| {
        block_on(begin_immediate_query().execute(&mut connection))?;
        let current = block_on(async {
            sqlx::query_file_scalar!("sql/metadata/load.sql", key)
                .fetch_optional(&mut connection)
                .await
        })?;
        let replacement = update(current).map_err(|value| DatabaseError::InvalidValue {
            field: "metadata update",
            value,
        })?;
        if let Some(value) = replacement {
            block_on(async {
                sqlx::query_file!("sql/metadata/upsert.sql", key, value)
                    .execute(&mut connection)
                    .await?;
                Ok(())
            })?;
        } else {
            block_on(async {
                sqlx::query_file!("sql/metadata/delete.sql", key)
                    .execute(&mut connection)
                    .await?;
                Ok(())
            })?;
        }
        block_on(commit_query().execute(&mut connection))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = block_on(rollback_query().execute(&mut connection));
    }
    finish_connection(connection, result)
}

pub(crate) fn run_operator_query(path: &Path, query: &str) -> Result<Vec<Vec<String>>, String> {
    let mut connection = open_readonly(path).map_err(|error| error.to_string())?;
    let result = block_on(async {
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
    });
    finish_connection(connection, result).map_err(|error| error.to_string())
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

pub(super) fn finish_connection<T>(
    connection: SqliteConnection,
    result: Result<T, DatabaseError>,
) -> Result<T, DatabaseError> {
    crate::async_runtime::block_on(super::pools::close_connection(connection, result))
        .map_err(DatabaseError::Runtime)?
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
    connection: Option<SqliteConnection>,
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

    #[allow(
        dead_code,
        reason = "focused persistence tests inspect transaction paths"
    )]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn execute(
        &self,
        statement: &str,
        values: impl AsRef<[TestValue]>,
    ) -> Result<u64, String> {
        let mut connection = connect_writable(&self.path).map_err(|error| error.to_string())?;
        let result = block_on(async {
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
        });
        finish_connection(connection, result).map_err(|error| error.to_string())
    }

    pub(crate) fn execute_batch(&self, statements: &str) -> Result<(), String> {
        let mut connection = connect_writable(&self.path).map_err(|error| error.to_string())?;
        let result = block_on(async {
            // SQLX_RUNTIME_SQL: test-only batch adapter receives fixture SQL from callers.
            sqlx::raw_sql(statements).execute(&mut connection).await?;
            Ok(())
        });
        finish_connection(connection, result).map_err(|error| error.to_string())
    }

    pub(crate) fn query_row<T>(
        &self,
        statement: &str,
        values: impl AsRef<[TestValue]>,
        map: impl FnOnce(&TestRow) -> Result<T, sqlx::Error>,
    ) -> Result<T, String> {
        let mut connection = connect_writable(&self.path).map_err(|error| error.to_string())?;
        let result = block_on(async {
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
        });
        let row = finish_connection(connection, result).map_err(|error| error.to_string())?;
        map(&TestRow(row)).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
impl TestConnection {
    pub(crate) fn open_writable(path: &Path) -> Result<Self, String> {
        super::storage::prepare_writable(path).map_err(|error| error.to_string())?;
        let connection = connect_writable(path).map_err(|error| error.to_string())?;
        Ok(Self {
            connection: Some(connection),
        })
    }

    pub(crate) fn open_readonly(path: &Path) -> Result<Self, String> {
        let connection = open_readonly(path).map_err(|error| error.to_string())?;
        Ok(Self {
            connection: Some(connection),
        })
    }

    pub(crate) fn execute_batch(&mut self, statements: &str) -> Result<(), String> {
        block_on(async {
            // SQLX_RUNTIME_SQL: test-only connection fixture receives SQL from test callers.
            sqlx::raw_sql(statements)
                .execute(self.connection.as_mut().expect("test connection is open"))
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
                .fetch_one(self.connection.as_mut().expect("test connection is open"))
                .await
        })
        .map_err(|error| error.to_string())
    }

    pub(crate) fn scalar_i64(&mut self, query: &str) -> Result<i64, String> {
        block_on(async {
            // SQLX_RUNTIME_SQL: test-only assertion helper receives fixture SQL from callers.
            sqlx::query_scalar::<_, i64>(query)
                .fetch_one(self.connection.as_mut().expect("test connection is open"))
                .await
        })
        .map_err(|error| error.to_string())
    }

    pub(crate) fn scalar_string(&mut self, query: &str) -> Result<String, String> {
        block_on(async {
            // SQLX_RUNTIME_SQL: test-only assertion helper receives fixture SQL from callers.
            sqlx::query_scalar::<_, String>(query)
                .fetch_one(self.connection.as_mut().expect("test connection is open"))
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
                .fetch_one(self.connection.as_mut().expect("test connection is open"))
                .await
        })
        .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
impl Drop for TestConnection {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            let _ = crate::async_runtime::block_on(connection.close());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;

    static SEQUENCE: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn metadata_updates_serialize_the_complete_read_modify_write() {
        let path = std::env::temp_dir().join(format!(
            "prism-metadata-update-{}-{}.db",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        upsert_metadata(&path, "test-key", "first").unwrap();
        let (first_started_tx, first_started_rx) = mpsc::channel();
        let (release_first_tx, release_first_rx) = mpsc::channel();
        let first_path = path.clone();
        let first = std::thread::spawn(move || {
            update_metadata(&first_path, "test-key", |current| {
                assert_eq!(current.as_deref(), Some("first"));
                first_started_tx.send(()).unwrap();
                release_first_rx.recv().unwrap();
                Ok(None)
            })
            .unwrap();
        });
        first_started_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();

        let (second_started_tx, second_started_rx) = mpsc::channel();
        let second_path = path.clone();
        let second = std::thread::spawn(move || {
            update_metadata(&second_path, "test-key", |current| {
                assert!(current.is_none());
                second_started_tx.send(()).unwrap();
                Ok(Some("second".to_string()))
            })
            .unwrap();
        });

        assert!(
            second_started_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "a concurrent updater entered before the first transaction committed"
        );
        release_first_tx.send(()).unwrap();
        first.join().unwrap();
        second_started_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        second.join().unwrap();
        assert_eq!(
            load_metadata(&path, "test-key").unwrap().as_deref(),
            Some("second")
        );

        remove_database(&path);
    }

    fn remove_database(path: &Path) {
        let _ = std::fs::remove_file(path);
        for suffix in ["-wal", "-shm"] {
            let mut sidecar = path.as_os_str().to_os_string();
            sidecar.push(suffix);
            let _ = std::fs::remove_file(std::path::PathBuf::from(sidecar));
        }
    }
}
