use std::path::PathBuf;
use std::time::{Duration, Instant};

use prism::{
    DefinitionSnapshot, ExecutionContext, LaunchWorkflow, StepFuture, StepImplementation,
    WorkerConfig, WorkflowOperations, WorkflowStep, WorkflowWorker,
};

struct BenchStep;

impl StepImplementation for BenchStep {
    fn execute<'a>(&'a self, context: ExecutionContext) -> StepFuture<'a> {
        Box::pin(async move {
            context
                .stdout(vec![b'x'; 1024])
                .await
                .map_err(|error| error.to_string())?;
            Ok("{}".into())
        })
    }
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .expect("benchmark runtime");
    runtime.block_on(run()).expect("workflow benchmark");
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    const RUNS: usize = 100;
    let path = benchmark_path();
    let operations = WorkflowOperations::open(&path).await?;
    operations
        .register_definition(DefinitionSnapshot {
            id: "benchmark-definition",
            name: "benchmark",
            revision: "1",
            source: "benchmark",
            trusted: true,
            body_json: "{}",
            digest: "benchmark-v1",
            now_unix_ms: 1,
        })
        .await?;

    let started = Instant::now();
    for index in 0..RUNS {
        let run_id = format!("benchmark-run-{index}");
        operations
            .launch_materialized(
                LaunchWorkflow {
                    run_id: &run_id,
                    definition_snapshot_id: "benchmark-definition",
                    repository: Some("/benchmark"),
                    idempotency_key: &run_id,
                    now_unix_ms: 2,
                },
                vec![WorkflowStep {
                    id: format!("benchmark-step-{index}"),
                    key: "execute".into(),
                    implementation: "benchmark".into(),
                    target_id: "local".into(),
                    input_json: "{}".into(),
                    dependencies: Vec::new(),
                    resources: Vec::new(),
                }],
            )
            .await?;
    }
    let launch_elapsed = started.elapsed();

    let scan_started = Instant::now();
    let projected = operations.list(Some("/benchmark"), 256).await?;
    let scan_elapsed = scan_started.elapsed();
    assert_eq!(projected.len(), RUNS);

    let mut worker = WorkflowWorker::open(
        &path,
        "benchmark-worker",
        WorkerConfig {
            agent_capacity: 8,
            command_capacity: 8,
            provider_capacity: 8,
            target_capacity: 8,
            repository_capacity: 8,
            dispatch_capacity: 16,
            scheduler_batch: 16,
            scheduler_interval: Duration::from_millis(1),
            output_flush_interval: Duration::from_millis(1),
            ..WorkerConfig::default()
        },
    )
    .await?;
    worker.register_as("benchmark", prism::ExecutionClass::Command, BenchStep)?;
    let (shutdown, receiver) = tokio::sync::watch::channel(false);
    let execution_started = Instant::now();
    let worker_task = tokio::spawn(worker.run(receiver));
    loop {
        let runs = operations.list(Some("/benchmark"), 256).await?;
        if runs.iter().all(|run| run.status == "succeeded") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let execution_elapsed = execution_started.elapsed();
    shutdown.send(true)?;
    worker_task.await??;

    let bytes = std::fs::metadata(&path)?.len();
    let metrics = operations.control_plane_metrics().await?;
    let writer_wait = metrics
        .iter()
        .find(|metric| metric.name == "writer_wait_us")
        .map_or(0, |metric| metric.value);
    println!("launch: {RUNS} runs in {launch_elapsed:?}");
    println!("scheduler projection: {RUNS} runs in {scan_elapsed:?}");
    println!("claim/execute/output: {RUNS} attempts in {execution_elapsed:?}");
    println!("latest writer wait: {writer_wait}us");
    println!("database growth: {bytes} bytes");

    remove_database(&path);
    Ok(())
}

fn benchmark_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "prism-workflow-benchmark-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

fn remove_database(path: &std::path::Path) {
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        let _ = std::fs::remove_file(candidate);
    }
}
