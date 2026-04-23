use std::{
    collections::{HashMap, HashSet, VecDeque},
    env,
    net::SocketAddr,
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive, Sse},
        Html,
    },
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use futures::{stream, Stream, StreamExt};
use hmac::{Hmac, Mac};
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use sqlx::{postgres::PgPoolOptions, PgPool, Row};
use tokio::{
    fs,
    io::{AsyncBufReadExt, BufReader},
    process::Command,
    sync::{broadcast, mpsc, Mutex, RwLock},
    task::JoinSet,
    time::sleep,
};
use tokio_stream::wrappers::BroadcastStream;
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{error, info, warn};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Deserialize)]
struct AppConfig {
    server: ServerConfig,
    security: SecurityConfig,
    auth: Option<AuthConfig>,
    runner: RunnerConfig,
    database: Option<DatabaseConfig>,
    projects: Vec<ProjectConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct ServerConfig {
    listen: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SecurityConfig {
    github_webhook_secret: String,
}

#[derive(Debug, Clone, Deserialize)]
struct AuthConfig {
    api_token: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RunnerConfig {
    workspace_root: String,
    max_parallel_jobs: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct DatabaseConfig {
    url: String,
    max_connections: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct ProjectConfig {
    id: String,
    repository: String,
    branch: String,
    workdir: String,
    image: String,
    stable_tag: Option<String>,
    portainer_webhook: Option<String>,
    healthcheck_url: Option<String>,
    healthcheck_timeout_sec: Option<u64>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    steps: Vec<StepConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct StepConfig {
    name: String,
    run: String,
    shell: Option<String>,
    timeout_sec: Option<u64>,
    only_actions: Option<Vec<String>>,
    skip_actions: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JobStatus {
    Queued,
    Running,
    Success,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JobAction {
    Webhook,
    Retry,
    Manual,
    Rollback,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Job {
    id: Uuid,
    project_id: String,
    repository: String,
    branch: String,
    sha: String,
    short_sha: String,
    delivery_id: Option<String>,
    source_deployment_id: Option<i64>,
    action: JobAction,
    status: JobStatus,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    current_stage: Option<String>,
    error: Option<String>,
    image: String,
    image_reference: String,
    image_digest: Option<String>,
    stable_tag: Option<String>,
    portainer_webhook: Option<String>,
    logs: Vec<JobLogEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobLogEntry {
    ts: DateTime<Utc>,
    level: String,
    stage: String,
    message: String,
}

#[derive(Debug, Clone, Serialize)]
struct JobSummary {
    id: Uuid,
    project_id: String,
    repository: String,
    branch: String,
    sha: String,
    image_reference: String,
    image_digest: Option<String>,
    status: JobStatus,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    current_stage: Option<String>,
    error: Option<String>,
}

impl From<&Job> for JobSummary {
    fn from(value: &Job) -> Self {
        Self {
            id: value.id,
            project_id: value.project_id.clone(),
            repository: value.repository.clone(),
            branch: value.branch.clone(),
            sha: value.sha.clone(),
            image_reference: value.image_reference.clone(),
            image_digest: value.image_digest.clone(),
            status: value.status.clone(),
            created_at: value.created_at,
            started_at: value.started_at,
            finished_at: value.finished_at,
            current_stage: value.current_stage.clone(),
            error: value.error.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct DashboardResponse {
    jobs: Vec<JobSummary>,
    deployments: Vec<Deployment>,
    projects: Vec<ProjectConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DeploymentStatus {
    Success,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Deployment {
    id: i64,
    job_id: Uuid,
    project_id: String,
    repository: String,
    branch: String,
    sha: String,
    image: String,
    image_reference: String,
    image_digest: Option<String>,
    stable_tag: Option<String>,
    status: DeploymentStatus,
    deployed_at: DateTime<Utc>,
    healthcheck_url: Option<String>,
    healthcheck_status: Option<u16>,
    healthcheck_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GithubPushEvent {
    #[serde(default)]
    after: String,
    #[serde(rename = "ref")]
    ref_name: String,
    repository: GithubRepository,
}

#[derive(Debug, Deserialize)]
struct GithubRepository {
    full_name: String,
}

#[derive(Debug, Serialize)]
struct EnqueueResponse {
    accepted: bool,
    job_id: Option<Uuid>,
    message: String,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    ok: bool,
    queued: usize,
    running: usize,
}

#[derive(Debug, Deserialize, Default)]
struct EventStreamQuery {
    token: Option<String>,
}

#[derive(Default)]
struct StateStore {
    jobs: HashMap<Uuid, Job>,
    order: VecDeque<Uuid>,
    active_locks: HashSet<String>,
}

struct AppState {
    config: AppConfig,
    store: RwLock<StateStore>,
    queue_tx: mpsc::Sender<Uuid>,
    log_channels: RwLock<HashMap<Uuid, broadcast::Sender<JobLogEntry>>>,
    cancel_flags: RwLock<HashMap<Uuid, Arc<AtomicBool>>>,
    db: Option<PgStore>,
    http: Client,
}

#[derive(Clone)]
struct PgStore {
    pool: PgPool,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config_path = env::var("CICD_CONFIG").unwrap_or_else(|_| "config/app.toml".to_string());
    let config = load_config(&config_path).await?;
    let listen: SocketAddr = config
        .server
        .listen
        .parse()
        .with_context(|| format!("invalid listen address: {}", config.server.listen))?;
    let db = init_db(config.database.clone()).await?;
    let restored_store = restore_store(db.as_ref()).await?;

    let (queue_tx, queue_rx) = mpsc::channel(256);
    let state = Arc::new(AppState {
        config: config.clone(),
        store: RwLock::new(restored_store),
        queue_tx,
        log_channels: RwLock::new(HashMap::new()),
        cancel_flags: RwLock::new(HashMap::new()),
        db,
        http: Client::new(),
    });
    sync_projects(&state).await?;

    spawn_workers(state.clone(), queue_rx);

    let app = Router::new()
        .route("/", get(index_html))
        .route("/healthz", get(health))
        .merge(
            Router::new()
                .route("/api/dashboard", get(dashboard))
                .route("/api/projects", get(list_projects))
                .route("/api/projects/:id/redeploy", post(redeploy_project))
                .route("/api/jobs", get(list_jobs))
                .route("/api/jobs/:id", get(get_job))
                .route("/api/jobs/:id/cancel", post(cancel_job))
                .route("/api/jobs/:id/retry", post(retry_job))
                .route("/api/jobs/:id/events", get(job_events))
                .route("/api/deployments", get(list_deployments))
                .route("/api/deployments/:id/rollback", post(rollback_deployment))
                .layer(middleware::from_fn_with_state(
                    state.clone(),
                    require_api_auth,
                )),
        )
        .route("/api/webhooks/github", post(github_webhook))
        .with_state(state)
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http());

    info!("listening on {}", listen);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn init_tracing() {
    let filter = env::var("RUST_LOG").unwrap_or_else(|_| "info,tower_http=info".to_string());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

async fn load_config(path: &str) -> Result<AppConfig> {
    let raw = fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read config: {path}"))?;
    toml::from_str(&raw).with_context(|| format!("failed to parse config: {path}"))
}

async fn init_db(config: Option<DatabaseConfig>) -> Result<Option<PgStore>> {
    let Some(config) = config else {
        return Ok(None);
    };

    let pool = PgPoolOptions::new()
        .max_connections(config.max_connections.unwrap_or(5))
        .connect(&config.url)
        .await
        .map_err(|err| map_db_connect_error(err, &config.url))?;

    let store = PgStore { pool };
    store.bootstrap().await?;

    Ok(Some(store))
}

fn map_db_connect_error(err: sqlx::Error, database_url: &str) -> anyhow::Error {
    if let sqlx::Error::Database(db_err) = &err {
        if db_err.code().as_deref() == Some("3D000") {
            let db_name = extract_database_name(database_url).unwrap_or("<unknown>");
            return anyhow!(
                "failed to connect to postgres: database \"{db_name}\" does not exist. \
create it first (for example: createdb {db_name}) or update [database].url in config"
            );
        }
    }

    anyhow!(err).context("failed to connect to postgres")
}

fn extract_database_name(database_url: &str) -> Option<&str> {
    let trimmed = database_url
        .split_once('?')
        .map_or(database_url, |(head, _)| head);
    trimmed.rsplit('/').next().filter(|name| !name.is_empty())
}

async fn restore_store(db: Option<&PgStore>) -> Result<StateStore> {
    let Some(db) = db else {
        return Ok(StateStore::default());
    };

    let jobs = db.load_recent_jobs(100).await?;
    let mut store = StateStore::default();
    for job in jobs {
        let id = job.id;
        if matches!(job.status, JobStatus::Running) {
            continue;
        }
        store.order.push_back(id);
        store.jobs.insert(id, job);
    }

    Ok(store)
}

async fn sync_projects(state: &AppState) -> Result<()> {
    let Some(db) = &state.db else {
        return Ok(());
    };

    db.sync_projects(&state.config.projects).await
}

fn spawn_workers(state: Arc<AppState>, queue_rx: mpsc::Receiver<Uuid>) {
    let parallelism = state.config.runner.max_parallel_jobs.max(1);
    let shared_rx = Arc::new(Mutex::new(queue_rx));
    for idx in 0..parallelism {
        let state = state.clone();
        let shared_rx = shared_rx.clone();
        tokio::spawn(async move {
            worker_loop(idx, state, shared_rx).await;
        });
    }
}

async fn worker_loop(
    worker_id: usize,
    state: Arc<AppState>,
    queue_rx: Arc<Mutex<mpsc::Receiver<Uuid>>>,
) {
    loop {
        let job_id = {
            let mut rx = queue_rx.lock().await;
            rx.recv().await
        };

        let Some(job_id) = job_id else {
            warn!("worker #{worker_id} stopped: queue closed");
            break;
        };

        if let Err(err) = run_job(state.clone(), job_id).await {
            error!(job_id = %job_id, error = %err, "job failed with internal error");
            fail_job(&state, job_id, format!("internal error: {err:#}")).await;
        }
    }
}

async fn run_job(state: Arc<AppState>, job_id: Uuid) -> Result<()> {
    let (project, repo_lock, cancel_flag) = {
        let mut store = state.store.write().await;
        let job = store
            .jobs
            .get(&job_id)
            .ok_or_else(|| anyhow!("job not found: {job_id}"))?;

        if matches!(job.status, JobStatus::Cancelled) {
            return Ok(());
        }

        let project_id = job.project_id.clone();
        let repository = job.repository.clone();
        let branch = job.branch.clone();

        let project = state
            .config
            .projects
            .iter()
            .find(|item| item.id == project_id)
            .cloned()
            .ok_or_else(|| anyhow!("project config missing: {}", project_id))?;

        let repo_lock = format!("{repository}:{branch}");
        if !store.active_locks.insert(repo_lock.clone()) {
            if let Some(job) = store.jobs.get_mut(&job_id) {
                job.current_stage = Some("waiting_lock".to_string());
            }
            drop(store);
            persist_job_snapshot(&state, job_id).await;
            sleep(Duration::from_secs(2)).await;
            state.queue_tx.send(job_id).await?;
            return Ok(());
        }

        if let Some(job) = store.jobs.get_mut(&job_id) {
            job.status = JobStatus::Running;
            job.started_at = Some(Utc::now());
            job.current_stage = Some("preparing".to_string());
        }

        (project, repo_lock, Arc::new(AtomicBool::new(false)))
    };
    state
        .cancel_flags
        .write()
        .await
        .insert(job_id, cancel_flag.clone());
    persist_job_snapshot(&state, job_id).await;

    log_line(
        &state,
        job_id,
        "info",
        "worker",
        "Job picked by worker".to_string(),
    )
    .await;

    let pipeline_result =
        execute_pipeline(state.clone(), job_id, &project, cancel_flag.clone()).await;
    let (result, health_status, health_error) = match pipeline_result {
        Ok(()) => match run_healthcheck(state.clone(), job_id, &project).await {
            Ok(status) => (Ok(()), status, None),
            Err(err) => {
                let message = err.to_string();
                (Err(err), None, Some(message))
            }
        },
        Err(err) => {
            let message = err.to_string();
            (Err(err), None, Some(message))
        }
    };
    hydrate_job_artifact_metadata(&state, job_id).await;

    {
        let mut store = state.store.write().await;
        store.active_locks.remove(&repo_lock);
    }
    state.cancel_flags.write().await.remove(&job_id);

    match result {
        Ok(()) => {
            {
                let mut store = state.store.write().await;
                if let Some(job) = store.jobs.get_mut(&job_id) {
                    job.status = JobStatus::Success;
                    job.finished_at = Some(Utc::now());
                    job.current_stage = Some("completed".to_string());
                }
            }
            log_line(
                &state,
                job_id,
                "info",
                "system",
                "Job finished successfully".to_string(),
            )
            .await;
            persist_job_snapshot(&state, job_id).await;
            persist_deployment_record(
                &state,
                job_id,
                DeploymentStatus::Success,
                health_status,
                None,
            )
            .await;
        }
        Err(err) => {
            let status = if cancel_flag.load(Ordering::SeqCst) {
                JobStatus::Cancelled
            } else {
                JobStatus::Failed
            };

            {
                let mut store = state.store.write().await;
                if let Some(job) = store.jobs.get_mut(&job_id) {
                    job.status = status.clone();
                    job.finished_at = Some(Utc::now());
                    job.error = Some(err.to_string());
                }
            }
            let level = if matches!(status, JobStatus::Cancelled) {
                "warn"
            } else {
                "error"
            };
            log_line(
                &state,
                job_id,
                level,
                "system",
                format!("Job finished with {:?}: {err}", status),
            )
            .await;
            persist_job_snapshot(&state, job_id).await;
            if matches!(status, JobStatus::Failed) {
                persist_deployment_record(
                    &state,
                    job_id,
                    DeploymentStatus::Failed,
                    health_status,
                    health_error.or_else(|| Some(err.to_string())),
                )
                .await;
            }
        }
    }

    Ok(())
}

async fn execute_pipeline(
    state: Arc<AppState>,
    job_id: Uuid,
    project: &ProjectConfig,
    cancel_flag: Arc<AtomicBool>,
) -> Result<()> {
    let (repo, branch, sha, short_sha, action, image_reference, source_deployment_id) = {
        let store = state.store.read().await;
        let job = store
            .jobs
            .get(&job_id)
            .ok_or_else(|| anyhow!("job missing"))?;
        (
            job.repository.clone(),
            job.branch.clone(),
            job.sha.clone(),
            job.short_sha.clone(),
            job.action.clone(),
            job.image_reference.clone(),
            job.source_deployment_id,
        )
    };

    let workspace_root = PathBuf::from(&state.config.runner.workspace_root);
    let project_dir = workspace_root.join(&project.workdir);
    fs::create_dir_all(&project_dir).await.with_context(|| {
        format!(
            "failed to create project workdir: {}",
            project_dir.to_string_lossy()
        )
    })?;

    for step in &project.steps {
        if !step_matches_action(step, &action) {
            log_line(
                &state,
                job_id,
                "info",
                "planner",
                format!(
                    "Skipping step '{}' for action {}",
                    step.name,
                    action.as_str()
                ),
            )
            .await;
            continue;
        }

        if cancel_flag.load(Ordering::SeqCst) {
            return Err(anyhow!("job cancelled before step {}", step.name));
        }

        {
            let mut store = state.store.write().await;
            if let Some(job) = store.jobs.get_mut(&job_id) {
                job.current_stage = Some(step.name.clone());
            }
        }
        persist_job_snapshot(&state, job_id).await;

        log_line(
            &state,
            job_id,
            "info",
            &step.name,
            format!("Running: {}", step.run),
        )
        .await;

        let shell = step.shell.clone().unwrap_or_else(|| "/bin/sh".to_string());
        let expanded = expand_command(
            &step.run,
            project,
            &repo,
            &branch,
            &sha,
            &short_sha,
            action.as_str(),
            &image_reference,
            source_deployment_id,
        );
        let timeout = Duration::from_secs(step.timeout_sec.unwrap_or(1800));

        run_shell_step(
            state.clone(),
            job_id,
            &step.name,
            &shell,
            &expanded,
            &project_dir,
            project,
            timeout,
            cancel_flag.clone(),
        )
        .await
        .with_context(|| format!("step '{}' failed", step.name))?;
    }

    Ok(())
}

fn step_matches_action(step: &StepConfig, action: &JobAction) -> bool {
    let current = action.as_str();

    if let Some(only) = &step.only_actions {
        if !only.iter().any(|item| item == current) {
            return false;
        }
    }

    if let Some(skip) = &step.skip_actions {
        if skip.iter().any(|item| item == current) {
            return false;
        }
    }

    true
}

#[allow(clippy::too_many_arguments)]
async fn run_shell_step(
    state: Arc<AppState>,
    job_id: Uuid,
    stage: &str,
    shell: &str,
    command: &str,
    workdir: &PathBuf,
    project: &ProjectConfig,
    timeout: Duration,
    cancel_flag: Arc<AtomicBool>,
) -> Result<()> {
    let mut child = Command::new(shell);
    child
        .arg("-lc")
        .arg(command)
        .current_dir(workdir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    for (key, value) in &project.env {
        child.env(key, value);
    }

    let mut child = child.spawn().with_context(|| {
        format!(
            "failed to start shell '{}' in {}",
            shell,
            workdir.to_string_lossy()
        )
    })?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let mut readers = JoinSet::new();
    if let Some(stdout) = stdout {
        let state = state.clone();
        let stage = stage.to_string();
        readers.spawn(async move {
            stream_reader(state, job_id, stage, "info", stdout).await;
        });
    }
    if let Some(stderr) = stderr {
        let state = state.clone();
        let stage = stage.to_string();
        readers.spawn(async move {
            stream_reader(state, job_id, stage, "warn", stderr).await;
        });
    }

    let started = tokio::time::Instant::now();
    loop {
        if cancel_flag.load(Ordering::SeqCst) {
            let _ = child.start_kill();
            while readers.join_next().await.is_some() {}
            return Err(anyhow!("cancel requested"));
        }

        if started.elapsed() >= timeout {
            let _ = child.start_kill();
            while readers.join_next().await.is_some() {}
            return Err(anyhow!("step timeout after {}s", timeout.as_secs()));
        }

        if let Some(status) = child.try_wait()? {
            while readers.join_next().await.is_some() {}
            if status.success() {
                return Ok(());
            }
            return Err(anyhow!("command exited with status {status}"));
        }

        sleep(Duration::from_millis(250)).await;
    }
}

async fn stream_reader(
    state: Arc<AppState>,
    job_id: Uuid,
    stage: String,
    level: &'static str,
    stream: impl tokio::io::AsyncRead + Unpin,
) {
    let mut reader = BufReader::new(stream).lines();
    loop {
        match reader.next_line().await {
            Ok(Some(line)) => {
                log_line(&state, job_id, level, &stage, line).await;
            }
            Ok(None) => break,
            Err(err) => {
                log_line(
                    &state,
                    job_id,
                    "error",
                    &stage,
                    format!("log stream error: {err}"),
                )
                .await;
                break;
            }
        }
    }
}

fn expand_command(
    template: &str,
    project: &ProjectConfig,
    repo: &str,
    branch: &str,
    sha: &str,
    short_sha: &str,
    action: &str,
    image_reference: &str,
    source_deployment_id: Option<i64>,
) -> String {
    let stable_tag = project
        .stable_tag
        .clone()
        .unwrap_or_else(|| branch.replace('/', "-"));
    let stable_image_reference = format!("{}:{}", project.image, stable_tag);
    let source_deployment = source_deployment_id
        .map(|value| value.to_string())
        .unwrap_or_default();

    template
        .replace("$REPOSITORY", repo)
        .replace("$BRANCH", branch)
        .replace("$SHA", sha)
        .replace("$SHORT_SHA", short_sha)
        .replace("$JOB_ACTION", action)
        .replace("$SOURCE_DEPLOYMENT_ID", &source_deployment)
        .replace("$PROJECT_ID", &project.id)
        .replace("$IMAGE", &project.image)
        .replace("$IMAGE_REFERENCE", image_reference)
        .replace("$STABLE_TAG", &stable_tag)
        .replace("$STABLE_IMAGE_REFERENCE", &stable_image_reference)
        .replace(
            "$PORTAINER_WEBHOOK",
            project.portainer_webhook.as_deref().unwrap_or(""),
        )
}

async fn log_line(state: &AppState, job_id: Uuid, level: &str, stage: &str, message: String) {
    let entry = JobLogEntry {
        ts: Utc::now(),
        level: level.to_string(),
        stage: stage.to_string(),
        message,
    };

    {
        let mut store = state.store.write().await;
        if let Some(job) = store.jobs.get_mut(&job_id) {
            job.logs.push(entry.clone());
        }
    }
    persist_log_entry(state, job_id, &entry).await;

    let sender = {
        let mut channels = state.log_channels.write().await;
        channels
            .entry(job_id)
            .or_insert_with(|| broadcast::channel(512).0)
            .clone()
    };
    let _ = sender.send(entry);
}

async fn fail_job(state: &AppState, job_id: Uuid, error: String) {
    let mut store = state.store.write().await;
    if let Some(job) = store.jobs.get_mut(&job_id) {
        job.status = JobStatus::Failed;
        job.finished_at = Some(Utc::now());
        job.error = Some(error);
    }
    drop(store);
    persist_job_snapshot(state, job_id).await;
}

async fn run_healthcheck(
    state: Arc<AppState>,
    job_id: Uuid,
    project: &ProjectConfig,
) -> Result<Option<u16>> {
    let Some(url) = &project.healthcheck_url else {
        return Ok(None);
    };

    let timeout = Duration::from_secs(project.healthcheck_timeout_sec.unwrap_or(60));
    let started = tokio::time::Instant::now();
    let mut last_error = None;

    log_line(
        &state,
        job_id,
        "info",
        "healthcheck",
        format!("Checking {}", url),
    )
    .await;

    while started.elapsed() < timeout {
        match state.http.get(url).send().await {
            Ok(response) => {
                let status = response.status().as_u16();
                if response.status().is_success() {
                    log_line(
                        &state,
                        job_id,
                        "info",
                        "healthcheck",
                        format!("Healthcheck passed with HTTP {}", status),
                    )
                    .await;
                    return Ok(Some(status));
                }

                let message = format!("Healthcheck returned HTTP {}", status);
                last_error = Some(message.clone());
                log_line(&state, job_id, "warn", "healthcheck", message).await;
            }
            Err(err) => {
                let message = format!("Healthcheck request failed: {err}");
                last_error = Some(message.clone());
                log_line(&state, job_id, "warn", "healthcheck", message).await;
            }
        }

        sleep(Duration::from_secs(3)).await;
    }

    Err(anyhow!(
        "{}",
        last_error.unwrap_or_else(|| "healthcheck timed out".to_string())
    ))
}

async fn index_html() -> Html<&'static str> {
    Html(include_str!("../../../web/index.html"))
}

async fn health(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let store = state.store.read().await;
    let queued = store
        .jobs
        .values()
        .filter(|job| matches!(job.status, JobStatus::Queued))
        .count();
    let running = store
        .jobs
        .values()
        .filter(|job| matches!(job.status, JobStatus::Running))
        .count();
    Json(HealthResponse {
        ok: true,
        queued,
        running,
    })
}

async fn require_api_auth(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next,
) -> Result<axum::response::Response, StatusCode> {
    if verify_api_auth(&state, &headers, None).is_ok() {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn verify_api_auth(
    state: &AppState,
    headers: &HeaderMap,
    token: Option<&str>,
) -> Result<(), StatusCode> {
    let Some(config) = &state.config.auth else {
        return Ok(());
    };

    let bearer = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    let api_key = headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim);
    let query_token = token.map(str::trim);

    let authorized = bearer == Some(config.api_token.as_str())
        || api_key == Some(config.api_token.as_str())
        || query_token == Some(config.api_token.as_str());

    if authorized {
        Ok(())
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

async fn dashboard(State(state): State<Arc<AppState>>) -> Json<DashboardResponse> {
    let store = state.store.read().await;
    let mut jobs = store
        .order
        .iter()
        .rev()
        .filter_map(|id| store.jobs.get(id))
        .map(JobSummary::from)
        .collect::<Vec<_>>();
    jobs.truncate(50);
    drop(store);

    let deployments = load_dashboard_deployments(&state).await;

    Json(DashboardResponse {
        jobs,
        deployments,
        projects: state.config.projects.clone(),
    })
}

async fn list_projects(State(state): State<Arc<AppState>>) -> Json<Vec<ProjectConfig>> {
    Json(state.config.projects.clone())
}

async fn list_deployments(State(state): State<Arc<AppState>>) -> Json<Vec<Deployment>> {
    Json(load_dashboard_deployments(&state).await)
}

async fn list_jobs(State(state): State<Arc<AppState>>) -> Json<Vec<JobSummary>> {
    let store = state.store.read().await;
    Json(
        store
            .order
            .iter()
            .rev()
            .filter_map(|id| store.jobs.get(id))
            .map(JobSummary::from)
            .collect(),
    )
}

async fn get_job(
    Path(id): Path<Uuid>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<Job>, StatusCode> {
    let store = state.store.read().await;
    store
        .jobs
        .get(&id)
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn cancel_job(
    Path(id): Path<Uuid>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<EnqueueResponse>, StatusCode> {
    let mut found = false;

    {
        let mut store = state.store.write().await;
        if let Some(job) = store.jobs.get_mut(&id) {
            found = true;
            if matches!(job.status, JobStatus::Queued) {
                job.status = JobStatus::Cancelled;
                job.finished_at = Some(Utc::now());
                job.current_stage = Some("cancelled".to_string());
            }
        }
    }
    persist_job_snapshot(&state, id).await;

    if !found {
        return Err(StatusCode::NOT_FOUND);
    }

    if let Some(flag) = state.cancel_flags.read().await.get(&id).cloned() {
        flag.store(true, Ordering::SeqCst);
        log_line(&state, id, "warn", "system", "Cancel requested".to_string()).await;
    }

    Ok(Json(EnqueueResponse {
        accepted: true,
        job_id: Some(id),
        message: "cancel requested".to_string(),
    }))
}

async fn retry_job(
    Path(id): Path<Uuid>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<EnqueueResponse>, StatusCode> {
    let retry_source = {
        let store = state.store.read().await;
        store.jobs.get(&id).cloned().ok_or(StatusCode::NOT_FOUND)?
    };

    let job_id = enqueue_job(
        state.clone(),
        &retry_source.project_id,
        &retry_source.repository,
        &retry_source.branch,
        &retry_source.sha,
        retry_source.delivery_id.clone(),
        JobAction::Retry,
        Some(retry_source.image_reference.clone()),
        retry_source.source_deployment_id,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(EnqueueResponse {
        accepted: true,
        job_id: Some(job_id),
        message: "retry queued".to_string(),
    }))
}

async fn redeploy_project(
    Path(project_id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<EnqueueResponse>, StatusCode> {
    let source = load_latest_successful_deployment(&state, &project_id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;

    let job_id = enqueue_job(
        state.clone(),
        &source.project_id,
        &source.repository,
        &source.branch,
        &source.sha,
        None,
        JobAction::Manual,
        Some(source.image_reference.clone()),
        Some(source.id),
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    log_line(
        &state,
        job_id,
        "info",
        "system",
        "Manual redeploy requested".to_string(),
    )
    .await;

    Ok(Json(EnqueueResponse {
        accepted: true,
        job_id: Some(job_id),
        message: "manual redeploy queued".to_string(),
    }))
}

async fn rollback_deployment(
    Path(deployment_id): Path<i64>,
    State(state): State<Arc<AppState>>,
) -> Result<Json<EnqueueResponse>, StatusCode> {
    let deployment = load_deployment_by_id(&state, deployment_id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;

    if !matches!(deployment.status, DeploymentStatus::Success) {
        return Ok(Json(EnqueueResponse {
            accepted: false,
            job_id: None,
            message: "only successful deployments can be rolled back".to_string(),
        }));
    }

    let job_id = enqueue_job(
        state.clone(),
        &deployment.project_id,
        &deployment.repository,
        &deployment.branch,
        &deployment.sha,
        None,
        JobAction::Rollback,
        Some(deployment.image_reference.clone()),
        Some(deployment.id),
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    log_line(
        &state,
        job_id,
        "warn",
        "system",
        format!(
            "Rollback requested from deployment #{} ({})",
            deployment_id, deployment.image_reference
        ),
    )
    .await;

    Ok(Json(EnqueueResponse {
        accepted: true,
        job_id: Some(job_id),
        message: "rollback queued".to_string(),
    }))
}

async fn github_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: String,
) -> Result<(StatusCode, Json<EnqueueResponse>), StatusCode> {
    validate_signature(
        &state.config.security.github_webhook_secret,
        &headers,
        body.as_bytes(),
    )
    .map_err(|err| {
        warn!("webhook rejected: {err}");
        StatusCode::UNAUTHORIZED
    })?;

    let event = headers
        .get("x-github-event")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if event != "push" {
        return Ok((
            StatusCode::ACCEPTED,
            Json(EnqueueResponse {
                accepted: false,
                job_id: None,
                message: format!("ignored event: {event}"),
            }),
        ));
    }

    let payload: GithubPushEvent =
        serde_json::from_str(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let branch = payload
        .ref_name
        .strip_prefix("refs/heads/")
        .unwrap_or(&payload.ref_name)
        .to_string();

    let project = state
        .config
        .projects
        .iter()
        .find(|project| {
            project.repository == payload.repository.full_name && project.branch == branch
        })
        .cloned();

    let Some(project) = project else {
        return Ok((
            StatusCode::ACCEPTED,
            Json(EnqueueResponse {
                accepted: false,
                job_id: None,
                message: format!(
                    "no project mapping for {}:{}",
                    payload.repository.full_name, branch
                ),
            }),
        ));
    };

    let delivery_id = headers
        .get("x-github-delivery")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    let job_id = enqueue_job(
        state,
        &project.id,
        &payload.repository.full_name,
        &branch,
        &payload.after,
        delivery_id,
        JobAction::Webhook,
        None,
        None,
    )
    .await
    .map_err(|err| {
        error!("failed to enqueue webhook job: {err:#}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok((
        StatusCode::ACCEPTED,
        Json(EnqueueResponse {
            accepted: true,
            job_id: Some(job_id),
            message: "job queued".to_string(),
        }),
    ))
}

async fn enqueue_job(
    state: Arc<AppState>,
    project_id: &str,
    repository: &str,
    branch: &str,
    sha: &str,
    delivery_id: Option<String>,
    action: JobAction,
    image_reference_override: Option<String>,
    source_deployment_id: Option<i64>,
) -> Result<Uuid> {
    let project = state
        .config
        .projects
        .iter()
        .find(|project| project.id == project_id)
        .ok_or_else(|| anyhow!("project missing: {project_id}"))?;

    let id = Uuid::new_v4();
    let short_sha = sha.chars().take(12).collect::<String>();
    let image_reference = image_reference_override
        .unwrap_or_else(|| immutable_image_reference(&project.image, &short_sha));
    let job = Job {
        id,
        project_id: project.id.clone(),
        repository: repository.to_string(),
        branch: branch.to_string(),
        sha: sha.to_string(),
        short_sha,
        delivery_id,
        source_deployment_id,
        action,
        status: JobStatus::Queued,
        created_at: Utc::now(),
        started_at: None,
        finished_at: None,
        current_stage: Some("queued".to_string()),
        error: None,
        image: project.image.clone(),
        image_reference,
        image_digest: None,
        stable_tag: project.stable_tag.clone(),
        portainer_webhook: project.portainer_webhook.clone(),
        logs: Vec::new(),
    };

    {
        let mut store = state.store.write().await;
        store.order.push_back(id);
        store.jobs.insert(id, job);
    }
    persist_job_snapshot(&state, id).await;

    log_line(
        &state,
        id,
        "info",
        "system",
        format!("Queued from {:?} for {}@{}", project_id, branch, sha),
    )
    .await;
    state.queue_tx.send(id).await?;

    Ok(id)
}

async fn job_events(
    Path(id): Path<Uuid>,
    Query(query): Query<EventStreamQuery>,
    headers: HeaderMap,
    State(state): State<Arc<AppState>>,
) -> Result<Sse<impl Stream<Item = Result<Event, axum::Error>>>, StatusCode> {
    verify_api_auth(&state, &headers, query.token.as_deref())?;

    let sender = {
        let existing = state.store.read().await.jobs.contains_key(&id);
        if !existing {
            return Err(StatusCode::NOT_FOUND);
        }

        let mut channels = state.log_channels.write().await;
        channels
            .entry(id)
            .or_insert_with(|| broadcast::channel(512).0)
            .clone()
    };

    let history = {
        let store = state.store.read().await;
        store
            .jobs
            .get(&id)
            .map(|job| job.logs.clone())
            .unwrap_or_default()
    };

    let history_stream = stream::iter(history.into_iter().map(|entry| {
        Ok::<_, axum::Error>(Event::default().event("log").json_data(entry).unwrap())
    }));

    let live_stream = BroadcastStream::new(sender.subscribe()).filter_map(|item| async move {
        match item {
            Ok(entry) => Some(Ok(Event::default().event("log").json_data(entry).unwrap())),
            Err(_) => None,
        }
    });

    Ok(Sse::new(history_stream.chain(live_stream)).keep_alive(KeepAlive::default()))
}

fn validate_signature(secret: &str, headers: &HeaderMap, body: &[u8]) -> Result<()> {
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| anyhow!("missing signature"))?;

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())?;
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let expected = format!("sha256={}", hex_encode(&digest));

    if expected == signature {
        Ok(())
    } else {
        Err(anyhow!("signature mismatch"))
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(HEX[(byte >> 4) as usize] as char);
        result.push(HEX[(byte & 0x0f) as usize] as char);
    }
    result
}

fn immutable_image_reference(image: &str, short_sha: &str) -> String {
    format!("{image}:sha-{short_sha}")
}

fn extract_digest_from_logs(entries: &[JobLogEntry]) -> Option<String> {
    entries
        .iter()
        .rev()
        .find_map(|entry| extract_digest_from_text(&entry.message))
}

fn extract_digest_from_text(value: &str) -> Option<String> {
    let regex = Regex::new(r"sha256:[0-9a-f]{64}").ok()?;
    regex.find(value).map(|item| item.as_str().to_string())
}

async fn persist_job_snapshot(state: &AppState, job_id: Uuid) {
    let Some(db) = &state.db else {
        return;
    };

    let snapshot = {
        let store = state.store.read().await;
        store.jobs.get(&job_id).cloned()
    };

    let Some(job) = snapshot else {
        return;
    };

    if let Err(err) = db.upsert_job(&job).await {
        error!(job_id = %job_id, error = %err, "failed to persist job");
    }
}

async fn hydrate_job_artifact_metadata(state: &AppState, job_id: Uuid) {
    let (image_reference, fallback_digest) = {
        let store = state.store.read().await;
        let Some(job) = store.jobs.get(&job_id) else {
            return;
        };
        (
            job.image_reference.clone(),
            extract_digest_from_logs(&job.logs),
        )
    };

    let digest = resolve_image_digest(state, job_id, &image_reference)
        .await
        .or(fallback_digest);

    if digest.is_none() {
        return;
    }

    {
        let mut store = state.store.write().await;
        if let Some(job) = store.jobs.get_mut(&job_id) {
            if job.image_digest.is_none() {
                job.image_digest = digest.clone();
            }
        }
    }

    persist_job_snapshot(state, job_id).await;
}

async fn resolve_image_digest(
    state: &AppState,
    job_id: Uuid,
    image_reference: &str,
) -> Option<String> {
    let output = Command::new("/bin/sh")
        .arg("-lc")
        .arg(format!(
            "docker buildx imagetools inspect '{}' 2>/dev/null || docker manifest inspect '{}' 2>/dev/null",
            image_reference, image_reference
        ))
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let digest = extract_digest_from_text(&stdout)?;
    log_line(
        state,
        job_id,
        "info",
        "artifact",
        format!("Resolved image digest {}", digest),
    )
    .await;
    Some(digest)
}

async fn persist_log_entry(state: &AppState, job_id: Uuid, entry: &JobLogEntry) {
    let Some(db) = &state.db else {
        return;
    };

    if let Err(err) = db.insert_log(job_id, entry).await {
        error!(job_id = %job_id, error = %err, "failed to persist job log");
    }
}

async fn persist_deployment_record(
    state: &AppState,
    job_id: Uuid,
    status: DeploymentStatus,
    healthcheck_status: Option<u16>,
    healthcheck_error: Option<String>,
) {
    let Some(db) = &state.db else {
        return;
    };

    let snapshot = {
        let store = state.store.read().await;
        store.jobs.get(&job_id).cloned()
    };

    let Some(job) = snapshot else {
        return;
    };

    let deployment = Deployment {
        id: 0,
        job_id,
        project_id: job.project_id.clone(),
        repository: job.repository.clone(),
        branch: job.branch.clone(),
        sha: job.sha.clone(),
        image: job.image.clone(),
        image_reference: job.image_reference.clone(),
        image_digest: job.image_digest.clone(),
        stable_tag: job.stable_tag.clone(),
        status,
        deployed_at: job.finished_at.unwrap_or_else(Utc::now),
        healthcheck_url: state
            .config
            .projects
            .iter()
            .find(|project| project.id == job.project_id)
            .and_then(|project| project.healthcheck_url.clone()),
        healthcheck_status,
        healthcheck_error,
    };

    if let Err(err) = db.insert_deployment(&deployment).await {
        error!(job_id = %job_id, error = %err, "failed to persist deployment");
    }
}

async fn load_dashboard_deployments(state: &AppState) -> Vec<Deployment> {
    let Some(db) = &state.db else {
        return Vec::new();
    };

    match db.load_recent_deployments(20).await {
        Ok(value) => value,
        Err(err) => {
            error!(error = %err, "failed to load deployments");
            Vec::new()
        }
    }
}

async fn load_deployment_by_id(state: &AppState, deployment_id: i64) -> Option<Deployment> {
    let Some(db) = &state.db else {
        return None;
    };

    match db.load_deployment_by_id(deployment_id).await {
        Ok(value) => value,
        Err(err) => {
            error!(deployment_id, error = %err, "failed to load deployment");
            None
        }
    }
}

async fn load_latest_successful_deployment(
    state: &AppState,
    project_id: &str,
) -> Option<Deployment> {
    let Some(db) = &state.db else {
        return None;
    };

    match db.load_latest_successful_deployment(project_id).await {
        Ok(value) => value,
        Err(err) => {
            error!(project_id, error = %err, "failed to load latest deployment");
            None
        }
    }
}

impl PgStore {
    async fn bootstrap(&self) -> Result<()> {
        sqlx::query(
            r#"
            create table if not exists projects (
              id text primary key,
              repository text not null,
              branch text not null,
              image text not null,
              stable_tag text,
              portainer_webhook text,
              created_at timestamptz not null default now()
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            create table if not exists jobs (
              id uuid primary key,
              project_id text not null references projects(id),
              repository text not null,
              branch text not null,
              sha text not null,
              short_sha text not null,
              delivery_id text,
              source_deployment_id bigint,
              status text not null,
              action text not null,
              created_at timestamptz not null,
              started_at timestamptz,
              finished_at timestamptz,
              current_stage text,
              error text,
              image text not null,
              image_reference text not null,
              image_digest text,
              stable_tag text,
              portainer_webhook text
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            create table if not exists job_logs (
              id bigserial primary key,
              job_id uuid not null references jobs(id) on delete cascade,
              ts timestamptz not null default now(),
              level text not null,
              stage text not null,
              message text not null
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query(
            r#"
            create table if not exists deployments (
              id bigserial primary key,
              job_id uuid not null references jobs(id) on delete cascade,
              project_id text not null references projects(id),
              repository text not null,
              branch text not null,
              sha text not null,
              image text not null,
              image_reference text not null,
              image_digest text,
              stable_tag text,
              status text not null,
              deployed_at timestamptz not null,
              healthcheck_url text,
              healthcheck_status integer,
              healthcheck_error text
            )
            "#,
        )
        .execute(&self.pool)
        .await?;

        sqlx::query("alter table jobs add column if not exists image_reference text")
            .execute(&self.pool)
            .await?;
        sqlx::query("alter table jobs add column if not exists source_deployment_id bigint")
            .execute(&self.pool)
            .await?;
        sqlx::query("alter table jobs add column if not exists image_digest text")
            .execute(&self.pool)
            .await?;
        sqlx::query("update jobs set image_reference = image || ':sha-' || short_sha where image_reference is null")
            .execute(&self.pool)
            .await?;
        sqlx::query("alter table jobs alter column image_reference set not null")
            .execute(&self.pool)
            .await?;

        sqlx::query("alter table deployments add column if not exists image_reference text")
            .execute(&self.pool)
            .await?;
        sqlx::query("alter table deployments add column if not exists image_digest text")
            .execute(&self.pool)
            .await?;
        sqlx::query("update deployments set image_reference = image || ':sha-' || substring(sha from 1 for 12) where image_reference is null")
            .execute(&self.pool)
            .await?;
        sqlx::query("alter table deployments alter column image_reference set not null")
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    async fn sync_projects(&self, projects: &[ProjectConfig]) -> Result<()> {
        for project in projects {
            sqlx::query(
                r#"
                insert into projects (id, repository, branch, image, stable_tag, portainer_webhook)
                values ($1, $2, $3, $4, $5, $6)
                on conflict (id) do update set
                  repository = excluded.repository,
                  branch = excluded.branch,
                  image = excluded.image,
                  stable_tag = excluded.stable_tag,
                  portainer_webhook = excluded.portainer_webhook
                "#,
            )
            .bind(&project.id)
            .bind(&project.repository)
            .bind(&project.branch)
            .bind(&project.image)
            .bind(project.stable_tag.clone())
            .bind(project.portainer_webhook.clone())
            .execute(&self.pool)
            .await?;
        }

        Ok(())
    }

    async fn upsert_job(&self, job: &Job) -> Result<()> {
        sqlx::query(
            r#"
            insert into jobs (
              id, project_id, repository, branch, sha, short_sha, delivery_id, source_deployment_id, status, action,
              created_at, started_at, finished_at, current_stage, error, image, image_reference, image_digest, stable_tag, portainer_webhook
            )
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)
            on conflict (id) do update set
              project_id = excluded.project_id,
              repository = excluded.repository,
              branch = excluded.branch,
              sha = excluded.sha,
              short_sha = excluded.short_sha,
              delivery_id = excluded.delivery_id,
              source_deployment_id = excluded.source_deployment_id,
              status = excluded.status,
              action = excluded.action,
              started_at = excluded.started_at,
              finished_at = excluded.finished_at,
              current_stage = excluded.current_stage,
              error = excluded.error,
              image = excluded.image,
              image_reference = excluded.image_reference,
              image_digest = excluded.image_digest,
              stable_tag = excluded.stable_tag,
              portainer_webhook = excluded.portainer_webhook
            "#,
        )
        .bind(job.id)
        .bind(&job.project_id)
        .bind(&job.repository)
        .bind(&job.branch)
        .bind(&job.sha)
        .bind(&job.short_sha)
        .bind(job.delivery_id.clone())
        .bind(job.source_deployment_id)
        .bind(job.status.as_str())
        .bind(job.action.as_str())
        .bind(job.created_at)
        .bind(job.started_at.clone())
        .bind(job.finished_at.clone())
        .bind(job.current_stage.clone())
        .bind(job.error.clone())
        .bind(&job.image)
        .bind(&job.image_reference)
        .bind(job.image_digest.clone())
        .bind(job.stable_tag.clone())
        .bind(job.portainer_webhook.clone())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn insert_log(&self, job_id: Uuid, entry: &JobLogEntry) -> Result<()> {
        sqlx::query(
            r#"
            insert into job_logs (job_id, ts, level, stage, message)
            values ($1, $2, $3, $4, $5)
            "#,
        )
        .bind(job_id)
        .bind(entry.ts)
        .bind(&entry.level)
        .bind(&entry.stage)
        .bind(&entry.message)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn insert_deployment(&self, deployment: &Deployment) -> Result<()> {
        sqlx::query(
            r#"
            insert into deployments (
              job_id, project_id, repository, branch, sha, image, image_reference, image_digest, stable_tag, status,
              deployed_at, healthcheck_url, healthcheck_status, healthcheck_error
            )
            values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
            "#,
        )
        .bind(deployment.job_id)
        .bind(&deployment.project_id)
        .bind(&deployment.repository)
        .bind(&deployment.branch)
        .bind(&deployment.sha)
        .bind(&deployment.image)
        .bind(&deployment.image_reference)
        .bind(deployment.image_digest.clone())
        .bind(deployment.stable_tag.clone())
        .bind(deployment.status.as_str())
        .bind(deployment.deployed_at)
        .bind(deployment.healthcheck_url.clone())
        .bind(deployment.healthcheck_status.map(i32::from))
        .bind(deployment.healthcheck_error.clone())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    async fn load_recent_jobs(&self, limit: i64) -> Result<Vec<Job>> {
        let rows = sqlx::query(
            r#"
            select
              id, project_id, repository, branch, sha, short_sha, delivery_id, source_deployment_id, status, action,
              created_at, started_at, finished_at, current_stage, error, image, image_reference, image_digest, stable_tag, portainer_webhook
            from jobs
            order by created_at desc
            limit $1
            "#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let mut jobs = Vec::with_capacity(rows.len());
        for row in rows {
            let id: Uuid = row.try_get("id")?;
            let logs = self.load_logs(id).await?;
            let status_raw: String = row.try_get("status")?;
            let action_raw: String = row.try_get("action")?;

            jobs.push(Job {
                id,
                project_id: row.try_get("project_id")?,
                repository: row.try_get("repository")?,
                branch: row.try_get("branch")?,
                sha: row.try_get("sha")?,
                short_sha: row.try_get("short_sha")?,
                delivery_id: row.try_get("delivery_id")?,
                source_deployment_id: row.try_get("source_deployment_id")?,
                action: JobAction::from_db(&action_raw)?,
                status: JobStatus::from_db(&status_raw)?,
                created_at: row.try_get("created_at")?,
                started_at: row.try_get("started_at")?,
                finished_at: row.try_get("finished_at")?,
                current_stage: row.try_get("current_stage")?,
                error: row.try_get("error")?,
                image: row.try_get("image")?,
                image_reference: row.try_get("image_reference")?,
                image_digest: row.try_get("image_digest")?,
                stable_tag: row.try_get("stable_tag")?,
                portainer_webhook: row.try_get("portainer_webhook")?,
                logs,
            });
        }

        jobs.reverse();
        Ok(jobs)
    }

    async fn load_logs(&self, job_id: Uuid) -> Result<Vec<JobLogEntry>> {
        let rows = sqlx::query(
            r#"
            select ts, level, stage, message
            from job_logs
            where job_id = $1
            order by ts asc, id asc
            "#,
        )
        .bind(job_id)
        .fetch_all(&self.pool)
        .await?;

        let mut logs = Vec::with_capacity(rows.len());
        for row in rows {
            logs.push(JobLogEntry {
                ts: row.try_get("ts")?,
                level: row.try_get("level")?,
                stage: row.try_get("stage")?,
                message: row.try_get("message")?,
            });
        }

        Ok(logs)
    }

    async fn load_recent_deployments(&self, limit: i64) -> Result<Vec<Deployment>> {
        let rows = sqlx::query(
            r#"
            select
              id, job_id, project_id, repository, branch, sha, image, image_reference, image_digest, stable_tag, status,
              deployed_at, healthcheck_url, healthcheck_status, healthcheck_error
            from deployments
            order by deployed_at desc, id desc
            limit $1
            "#,
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let mut deployments = Vec::with_capacity(rows.len());
        for row in rows {
            let status_raw: String = row.try_get("status")?;
            let health_status_raw: Option<i32> = row.try_get("healthcheck_status")?;
            deployments.push(Deployment {
                id: row.try_get("id")?,
                job_id: row.try_get("job_id")?,
                project_id: row.try_get("project_id")?,
                repository: row.try_get("repository")?,
                branch: row.try_get("branch")?,
                sha: row.try_get("sha")?,
                image: row.try_get("image")?,
                image_reference: row.try_get("image_reference")?,
                image_digest: row.try_get("image_digest")?,
                stable_tag: row.try_get("stable_tag")?,
                status: DeploymentStatus::from_db(&status_raw)?,
                deployed_at: row.try_get("deployed_at")?,
                healthcheck_url: row.try_get("healthcheck_url")?,
                healthcheck_status: health_status_raw.and_then(|value| u16::try_from(value).ok()),
                healthcheck_error: row.try_get("healthcheck_error")?,
            });
        }

        Ok(deployments)
    }

    async fn load_latest_successful_deployment(
        &self,
        project_id: &str,
    ) -> Result<Option<Deployment>> {
        let row = sqlx::query(
            r#"
            select
              id, job_id, project_id, repository, branch, sha, image, image_reference, image_digest, stable_tag, status,
              deployed_at, healthcheck_url, healthcheck_status, healthcheck_error
            from deployments
            where project_id = $1 and status = 'success'
            order by deployed_at desc, id desc
            limit 1
            "#,
        )
        .bind(project_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let status_raw: String = row.try_get("status")?;
        let health_status_raw: Option<i32> = row.try_get("healthcheck_status")?;

        Ok(Some(Deployment {
            id: row.try_get("id")?,
            job_id: row.try_get("job_id")?,
            project_id: row.try_get("project_id")?,
            repository: row.try_get("repository")?,
            branch: row.try_get("branch")?,
            sha: row.try_get("sha")?,
            image: row.try_get("image")?,
            image_reference: row.try_get("image_reference")?,
            image_digest: row.try_get("image_digest")?,
            stable_tag: row.try_get("stable_tag")?,
            status: DeploymentStatus::from_db(&status_raw)?,
            deployed_at: row.try_get("deployed_at")?,
            healthcheck_url: row.try_get("healthcheck_url")?,
            healthcheck_status: health_status_raw.and_then(|value| u16::try_from(value).ok()),
            healthcheck_error: row.try_get("healthcheck_error")?,
        }))
    }

    async fn load_deployment_by_id(&self, deployment_id: i64) -> Result<Option<Deployment>> {
        let row = sqlx::query(
            r#"
            select
              id, job_id, project_id, repository, branch, sha, image, image_reference, image_digest, stable_tag, status,
              deployed_at, healthcheck_url, healthcheck_status, healthcheck_error
            from deployments
            where id = $1
            "#,
        )
        .bind(deployment_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let status_raw: String = row.try_get("status")?;
        let health_status_raw: Option<i32> = row.try_get("healthcheck_status")?;

        Ok(Some(Deployment {
            id: row.try_get("id")?,
            job_id: row.try_get("job_id")?,
            project_id: row.try_get("project_id")?,
            repository: row.try_get("repository")?,
            branch: row.try_get("branch")?,
            sha: row.try_get("sha")?,
            image: row.try_get("image")?,
            image_reference: row.try_get("image_reference")?,
            image_digest: row.try_get("image_digest")?,
            stable_tag: row.try_get("stable_tag")?,
            status: DeploymentStatus::from_db(&status_raw)?,
            deployed_at: row.try_get("deployed_at")?,
            healthcheck_url: row.try_get("healthcheck_url")?,
            healthcheck_status: health_status_raw.and_then(|value| u16::try_from(value).ok()),
            healthcheck_error: row.try_get("healthcheck_error")?,
        }))
    }
}

impl JobStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn from_db(value: &str) -> Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "success" => Ok(Self::Success),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(anyhow!("unknown job status: {other}")),
        }
    }
}

impl JobAction {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Webhook => "webhook",
            Self::Retry => "retry",
            Self::Manual => "manual",
            Self::Rollback => "rollback",
        }
    }

    fn from_db(value: &str) -> Result<Self> {
        match value {
            "webhook" => Ok(Self::Webhook),
            "retry" => Ok(Self::Retry),
            "manual" => Ok(Self::Manual),
            "rollback" => Ok(Self::Rollback),
            other => Err(anyhow!("unknown job action: {other}")),
        }
    }
}

impl DeploymentStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
        }
    }

    fn from_db(value: &str) -> Result<Self> {
        match value {
            "success" => Ok(Self::Success),
            "failed" => Ok(Self::Failed),
            other => Err(anyhow!("unknown deployment status: {other}")),
        }
    }
}
