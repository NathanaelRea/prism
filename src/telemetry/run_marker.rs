use std::collections::BTreeMap;
#[cfg(test)]
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::params;

use crate::repo::Repository;

const MARKER_VERSION: u32 = 1;
const MARKER_SUFFIX: &str = "run";
const STARTUP_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

static MARKER_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static ACTIVE_RUNS: OnceLock<Mutex<BTreeMap<PathBuf, ActiveRun>>> = OnceLock::new();

#[derive(Clone, Debug)]
pub(crate) struct BeginOutcome {
    pub run_id: String,
    pub stale_run_ids: Vec<String>,
}

#[derive(Debug)]
struct ActiveRun {
    run_id: String,
    repo_root: String,
    db_path: PathBuf,
    marker_path: PathBuf,
    marker: File,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MarkerRecord {
    run_id: String,
    status: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkerState {
    Live,
    Stale,
    Clean,
}

pub(crate) fn begin(repo: &Repository, version: &str) -> Result<BeginOutcome, String> {
    let mut active = active_runs()
        .lock()
        .map_err(|_| "run marker state lock was poisoned".to_string())?;
    if let Some(existing) = active.get(&repo.root) {
        return Ok(BeginOutcome {
            run_id: existing.run_id.clone(),
            stale_run_ids: Vec::new(),
        });
    }

    let outcome = begin_new(repo, version)?;
    let result = BeginOutcome {
        run_id: outcome.run_id.clone(),
        stale_run_ids: outcome.stale_run_ids.clone(),
    };
    active.insert(repo.root.clone(), outcome.active_run);
    Ok(result)
}

pub(crate) fn finish_all(status: &str, error: Option<&str>) -> Vec<String> {
    let Ok(mut active) = active_runs().lock() else {
        return vec!["run marker state lock was poisoned".to_string()];
    };
    let runs = std::mem::take(&mut *active);
    drop(active);

    let mut errors = Vec::new();
    for (_, mut run) in runs {
        if let Err(finish_error) = finish_database_row(&run, status, error) {
            errors.push(finish_error);
        }
        if let Err(finish_error) =
            write_marker(&mut run.marker, &run.run_id, "complete", Some(status))
        {
            errors.push(format!(
                "complete run marker {}: {finish_error}",
                run.marker_path.display()
            ));
        }
    }
    errors
}

struct NewRun {
    active_run: ActiveRun,
    run_id: String,
    stale_run_ids: Vec<String>,
}

fn begin_new(repo: &Repository, version: &str) -> Result<NewRun, String> {
    let marker_dir = repo.prism_dir().join("run-markers");
    fs::create_dir_all(&marker_dir).map_err(|error| {
        format!(
            "create run marker directory {}: {error}",
            marker_dir.display()
        )
    })?;
    let startup_lock_path = marker_dir.join("startup.lock");
    let startup_lock = open_lock(&startup_lock_path)?;
    acquire_startup_lock(&startup_lock, &startup_lock_path)?;

    let markers = inspect_existing_markers(&marker_dir)?;
    let stale = markers
        .iter()
        .filter(|marker| marker.state == MarkerState::Stale)
        .cloned()
        .collect::<Vec<_>>();
    let db_path = repo.prism_dir().join("prism.db");
    if !stale.is_empty() {
        crate::storage::verify_unclean_database_readonly(&db_path).map_err(|error| {
            format!(
                "unclean prior Prism run detected for {}; refusing normal database use: {error}",
                repo.root.display()
            )
        })?;
    }

    // Opening here initializes or validates storage before the process marker is
    // published. No normal domain operation can run until begin() returns.
    let conn = crate::storage::open_writable(&db_path).map_err(|error| error.to_string())?;
    let run_id = new_run_id();
    let marker_path = marker_dir.join(format!("{run_id}.{MARKER_SUFFIX}"));
    let mut marker = create_marker(&marker_path)?;
    write_marker(&mut marker, &run_id, "running", None)
        .map_err(|error| format!("write run marker {}: {error}", marker_path.display()))?;
    crate::durability::sync_directory(&marker_dir, crate::durability::DurabilityIntent::Maximum)
        .map_err(|error| {
            format!(
                "sync run marker directory {}: {error}",
                marker_dir.display()
            )
        })?;

    let started = now_ms();
    let transaction = crate::flight_recorder::TransactionTrace::begin("run_marker.begin");
    let transaction_result = (|| -> rusqlite::Result<()> {
        conn.execute_batch("begin immediate")?;
        for stale_marker in &stale {
            if let Some(record) = &stale_marker.record {
                conn.execute(
                    "update startup_run
                     set time_finished_unix_ms = ?1, status = 'unclean',
                         error = 'process exited without completing its run marker'
                     where id = ?2 and status = 'running'",
                    params![started, record.run_id],
                )?;
            }
        }
        conn.execute(
            "insert into startup_run (
                id, time_started_unix_ms, time_finished_unix_ms, status, repo, version, error
             ) values (?1, ?2, null, 'running', ?3, ?4, null)",
            params![run_id, started, repo.root.display().to_string(), version],
        )?;
        conn.execute_batch("commit")
    })();
    if let Err(error) = transaction_result {
        let _ = conn.execute_batch("rollback");
        drop(marker);
        let _ = fs::remove_file(&marker_path);
        return Err(format!("record repository run {run_id}: {error}"));
    }
    transaction.committed();
    drop(conn);

    for existing in markers {
        if existing.state != MarkerState::Live {
            let _ = fs::remove_file(existing.path);
        }
    }
    let stale_run_ids = stale
        .into_iter()
        .filter_map(|marker| marker.record.as_ref().map(|record| record.run_id.clone()))
        .collect();
    Ok(NewRun {
        run_id: run_id.clone(),
        stale_run_ids,
        active_run: ActiveRun {
            run_id,
            repo_root: repo.root.display().to_string(),
            db_path,
            marker_path,
            marker,
        },
    })
}

#[derive(Clone, Debug)]
struct InspectedMarker {
    path: PathBuf,
    state: MarkerState,
    record: Option<MarkerRecord>,
}

fn inspect_existing_markers(marker_dir: &Path) -> Result<Vec<InspectedMarker>, String> {
    let entries = fs::read_dir(marker_dir).map_err(|error| {
        format!(
            "read run marker directory {}: {error}",
            marker_dir.display()
        )
    })?;
    let mut paths = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|error| format!("read run marker entry in {}: {error}", marker_dir.display()))?
            .path();
        if path.extension().and_then(|value| value.to_str()) == Some(MARKER_SUFFIX) {
            paths.push(path);
        }
    }
    paths.sort();

    let mut markers = Vec::new();
    for path in paths {
        let mut file = match OpenOptions::new().read(true).write(true).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("open run marker {}: {error}", path.display())),
        };
        let (state, record) = inspect_marker_file(&mut file)
            .map_err(|error| format!("inspect run marker {}: {error}", path.display()))?;
        markers.push(InspectedMarker {
            path,
            state,
            record,
        });
    }
    Ok(markers)
}

fn inspect_marker_file(file: &mut File) -> std::io::Result<(MarkerState, Option<MarkerRecord>)> {
    match file.try_lock() {
        Ok(()) => {
            file.seek(SeekFrom::Start(0))?;
            let mut contents = String::new();
            file.read_to_string(&mut contents)?;
            let record = parse_marker(&contents);
            let state = if record
                .as_ref()
                .is_some_and(|record| record.status == "complete")
            {
                MarkerState::Clean
            } else {
                MarkerState::Stale
            };
            Ok((state, record))
        }
        Err(fs::TryLockError::WouldBlock) => Ok((MarkerState::Live, None)),
        Err(fs::TryLockError::Error(error)) => Err(error),
    }
}

fn parse_marker(contents: &str) -> Option<MarkerRecord> {
    let version = marker_value(contents, "version")?.parse::<u32>().ok()?;
    if version != MARKER_VERSION {
        return None;
    }
    Some(MarkerRecord {
        run_id: marker_value(contents, "run_id")?.to_string(),
        status: marker_value(contents, "status")?.to_string(),
    })
}

fn marker_value<'a>(contents: &'a str, key: &str) -> Option<&'a str> {
    contents
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
}

fn create_marker(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    let file = options
        .open(path)
        .map_err(|error| format!("create run marker {}: {error}", path.display()))?;
    file.try_lock().map_err(|error| match error {
        fs::TryLockError::WouldBlock => {
            format!("new run marker {} was unexpectedly locked", path.display())
        }
        fs::TryLockError::Error(error) => {
            format!("lock new run marker {}: {error}", path.display())
        }
    })?;
    Ok(file)
}

fn open_lock(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| format!("open repository startup lock {}: {error}", path.display()))
}

fn acquire_startup_lock(file: &File, path: &Path) -> Result<(), String> {
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(fs::TryLockError::WouldBlock) if started.elapsed() < STARTUP_LOCK_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(fs::TryLockError::WouldBlock) => {
                return Err(format!(
                    "repository startup lock {} remained busy for {} ms",
                    path.display(),
                    STARTUP_LOCK_TIMEOUT.as_millis()
                ));
            }
            Err(fs::TryLockError::Error(error)) => {
                return Err(format!(
                    "lock repository startup {}: {error}",
                    path.display()
                ));
            }
        }
    }
}

fn write_marker(
    marker: &mut File,
    run_id: &str,
    marker_status: &str,
    exit_status: Option<&str>,
) -> std::io::Result<()> {
    marker.seek(SeekFrom::Start(0))?;
    marker.set_len(0)?;
    writeln!(marker, "version={MARKER_VERSION}")?;
    writeln!(marker, "run_id={run_id}")?;
    writeln!(marker, "status={marker_status}")?;
    writeln!(marker, "pid={}", std::process::id())?;
    if let Some(exit_status) = exit_status {
        writeln!(marker, "exit_status={exit_status}")?;
        writeln!(marker, "finished_unix_ms={}", now_ms())?;
    } else {
        writeln!(marker, "started_unix_ms={}", now_ms())?;
    }
    crate::durability::sync_file(marker, crate::durability::DurabilityIntent::Maximum)
        .map_err(crate::durability::FileSyncError::into_source)?;
    Ok(())
}

fn finish_database_row(run: &ActiveRun, status: &str, error: Option<&str>) -> Result<(), String> {
    let conn = crate::storage::open_writable(&run.db_path).map_err(|error| error.to_string())?;
    conn.execute(
        "update startup_run
         set time_finished_unix_ms = ?1, status = ?2, error = ?3
         where id = ?4 and status = 'running'",
        params![now_ms(), status, error, run.run_id],
    )
    .map_err(|error| {
        format!(
            "complete repository run {} for {}: {error}",
            run.run_id, run.repo_root
        )
    })?;
    Ok(())
}

fn active_runs() -> &'static Mutex<BTreeMap<PathBuf, ActiveRun>> {
    ACTIVE_RUNS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn new_run_id() -> String {
    format!(
        "run-{}-{}-{}",
        std::process::id(),
        now_ms().max(0),
        MARKER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlocked_running_marker_is_stale() {
        let (path, file) = marker_fixture("stale", "running");
        let mut file = file;

        assert_eq!(
            inspect_marker_file(&mut file).unwrap().0,
            MarkerState::Stale,
        );

        drop(file);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn locked_running_marker_is_live() {
        let (path, owner) = marker_fixture("live", "running");
        owner.try_lock().unwrap();
        let mut observer = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();

        assert_eq!(
            inspect_marker_file(&mut observer).unwrap().0,
            MarkerState::Live,
        );

        drop(observer);
        drop(owner);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn unlocked_complete_marker_is_clean() {
        let (path, file) = marker_fixture("clean", "complete");
        let mut file = file;

        assert_eq!(
            inspect_marker_file(&mut file).unwrap().0,
            MarkerState::Clean,
        );

        drop(file);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn stale_marker_requires_clean_quick_and_foreign_key_checks() {
        let temp = unique_temp_dir("integrity-failure");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let db_path = repo.prism_dir().join("prism.db");
        let conn = crate::storage::open_writable(&db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", false).unwrap();
        conn.execute(
            "insert into startup_phase (run_id, phase, time_started_unix_ms, status)
             values ('missing', 'fixture', 1, 'ok')",
            [],
        )
        .unwrap();
        drop(conn);
        write_unlocked_repo_marker(&repo, "stale-fixture", "running");

        let error = match begin_new(&repo, "test") {
            Ok(_) => panic!("foreign-key corruption unexpectedly passed the stale-run gate"),
            Err(error) => error,
        };

        assert!(error.contains("unclean prior Prism run detected"));
        assert!(error.contains("foreign_key_check"));
        assert!(
            repo.prism_dir()
                .join("run-markers/stale-fixture.run")
                .exists()
        );
        let _ = fs::remove_dir_all(temp);
    }

    #[test]
    fn healthy_stale_marker_is_recorded_unclean_before_new_run() {
        let temp = unique_temp_dir("healthy-stale");
        fs::create_dir_all(&temp).unwrap();
        let repo = Repository::with_config_dir_for_test(temp.clone(), temp.join("config"));
        let db_path = repo.prism_dir().join("prism.db");
        let conn = crate::storage::open_writable(&db_path).unwrap();
        conn.execute(
            "insert into startup_run (
                id, time_started_unix_ms, status, repo, version
             ) values ('stale-fixture', 1, 'running', '/repo', 'test')",
            [],
        )
        .unwrap();
        drop(conn);
        write_unlocked_repo_marker(&repo, "stale-fixture", "running");

        let run = begin_new(&repo, "test").unwrap();

        assert_eq!(run.stale_run_ids, ["stale-fixture"]);
        let conn = crate::storage::open_readonly(&db_path).unwrap();
        let stale_status = conn
            .query_row(
                "select status from startup_run where id = 'stale-fixture'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        let current_status = conn
            .query_row(
                "select status from startup_run where id = ?1",
                [&run.run_id],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(stale_status, "unclean");
        assert_eq!(current_status, "running");
        assert!(
            !repo
                .prism_dir()
                .join("run-markers/stale-fixture.run")
                .exists()
        );
        drop(conn);
        drop(run);
        let _ = fs::remove_dir_all(temp);
    }

    fn marker_fixture(label: &str, status: &str) -> (PathBuf, File) {
        let mut name = OsString::from("prism-run-marker-test-");
        name.push(format!(
            "{label}-{}-{}",
            std::process::id(),
            MARKER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let path = std::env::temp_dir().join(name);
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        write_marker(&mut file, "fixture", status, None).unwrap();
        (path, file)
    }

    fn write_unlocked_repo_marker(repo: &Repository, run_id: &str, status: &str) {
        let marker_dir = repo.prism_dir().join("run-markers");
        fs::create_dir_all(&marker_dir).unwrap();
        let path = marker_dir.join(format!("{run_id}.run"));
        let mut marker = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap();
        write_marker(&mut marker, run_id, status, None).unwrap();
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "prism-run-marker-{label}-{}-{}",
            std::process::id(),
            MARKER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
