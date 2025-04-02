use axum::{
    routing::{get, post},
    Router,
    response::{Html, IntoResponse, sse::{Event, Sse, KeepAlive}},
    extract::{State, Json},
};
use minijinja::{path_loader, Environment, context};
use minijinja_autoreload::AutoReloader;
use std::{net::SocketAddr, sync::Arc, collections::HashMap, time::Duration, collections::VecDeque};
use tokio::sync::{Mutex, broadcast};
use tower_http::{
    services::ServeDir,
    trace::{DefaultMakeSpan, TraceLayer},
};
use tower_livereload::LiveReloadLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use serde::{Serialize, Deserialize};
use uuid::Uuid;
use futures::stream::{self, Stream, StreamExt};
use rand::Rng;
use axum::http::StatusCode;
use sqlx::{migrate::MigrateDatabase, Sqlite, SqlitePool, FromRow, Row};
use dotenvy::dotenv;
use std::env;
use axum::debug_handler;
use chrono::{DateTime, Utc};
use reqwest;
use rand::rngs::StdRng;
use rand::SeedableRng;

// --- Data Structures ---

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SimulationParams {
    pub chart: String,
    pub node_count: u32,
    pub duration_secs: u32,
}

impl SimulationParams {
    // Removed unused history_key method
}

#[derive(Serialize, Deserialize, Debug, Clone, FromRow)]
pub struct ResourceCost {
    #[sqlx(rename = "cpu_cores")]
    pub cpu_cores: f32,
    #[sqlx(rename = "memory_gb")]
    pub memory_gb: f32,
}

#[derive(Serialize, Debug, Clone)]
struct ClusterUtilization {
    cpu_percent: f32,
    memory_percent: f32,
    total_cpu_cores: f32,
    total_memory_gb: f32,
    used_cpu_cores: f32,
    used_memory_gb: f32,
}

#[derive(Serialize, Debug, Clone)]
struct NamespaceUtilization {
    cpu_percent: f32,
    memory_percent: f32,
    allocated_cpu_cores: f32,
    allocated_memory_gb: f32,
    used_cpu_cores: f32,
    used_memory_gb: f32,
    cluster_total_cpu: f32,
    cluster_total_memory: f32,
}

#[derive(Serialize, Debug, Clone)]
struct LastFinishedSimulation {
    simulation_id: Uuid,
    params: SimulationParams,
    predicted_cost: ResourceCost,
    actual_cost: ResourceCost,
    finished_at: chrono::DateTime<chrono::Utc>,
    duration_secs: u32,
}

// Default implementation for ResourceCost
impl Default for ResourceCost {
    fn default() -> Self {
        Self {
            cpu_cores: 0.0,
            memory_gb: 0.0,
        }
    }
}

// Default implementation for ClusterUtilization
impl Default for ClusterUtilization {
    fn default() -> Self {
        Self {
            cpu_percent: 0.0,
            memory_percent: 0.0,
            total_cpu_cores: 1812.0, // Updated to match user's environment (1812 vCPUs)
            total_memory_gb: 2978.0, // 2.91 TiB converted to GB (2.91 * 1024)
            used_cpu_cores: 0.0,
            used_memory_gb: 0.0,
        }
    }
}

// Default implementation for NamespaceUtilization
impl Default for NamespaceUtilization {
    fn default() -> Self {
        Self {
            cpu_percent: 0.0,
            memory_percent: 0.0,
            allocated_cpu_cores: 256.0, // Assuming namespace has 256 cores allocated
            allocated_memory_gb: 1024.0, // Assuming 1TB of memory allocated to namespace
            used_cpu_cores: 0.0,
            used_memory_gb: 0.0,
            cluster_total_cpu: 0.0,
            cluster_total_memory: 0.0,
        }
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct QueuedSimulation {
    pub request_id: Uuid,
    pub params: SimulationParams,
    pub predicted_cost: ResourceCost,
}

// Define a new struct for resource usage snapshots
#[derive(Serialize, Clone, Debug)]
pub struct ResourceSnapshot {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub cpu_cores: f32,
    pub memory_gb: f32,
}

#[derive(Serialize, Clone, Debug)]
pub struct ActiveSimulation {
    pub simulation_id: Uuid,
    pub params: SimulationParams,
    pub predicted_cost: ResourceCost,
    pub actual_cost: ResourceCost,
    pub usage_snapshots: Vec<ResourceSnapshot>, // Historical snapshots during simulation
    pub last_snapshot_time: chrono::DateTime<chrono::Utc>, // Track when last snapshot was taken
}

// Add struct for history entries
#[derive(Serialize, FromRow, Debug, Clone)]
pub struct CostHistoryEntry {
    chart: String,
    node_count: u32,
    duration_secs: u32,
    cpu_cores: f32,
    memory_gb: f32,
    observed_at: DateTime<Utc>,
}

// Events to broadcast via SSE
#[derive(Serialize, Debug, Clone)]
#[serde(tag = "type", content = "data")]
enum AppEvent {
    QueueUpdated(Vec<QueuedSimulation>),
    ActiveUpdated(Vec<ActiveSimulation>),
    LastFinished(LastFinishedSimulation),
    ClusterUtilizationUpdated(ClusterUtilization),
    NamespaceUtilizationUpdated(NamespaceUtilization),
}

// Add this with other structs
#[derive(Serialize, Debug)]
struct HistoryEntry {
    chart: String,
    node_count: i64,
    duration_secs: i64,
    cpu_cores: f64,
    memory_gb: f64,
    observed_at: chrono::DateTime<chrono::Utc>,
}

// Structure to hold Kubernetes metrics
#[derive(Debug, Clone)]
pub struct KubernetesMetrics {
    // Cluster metrics
    pub cluster_total_cpu: f32,
    pub cluster_used_cpu: f32,
    pub cluster_total_memory_gb: f32,
    pub cluster_used_memory_gb: f32,
    
    // Namespace metrics (our simulations namespace)
    pub namespace: String,
    pub namespace_cpu_limits: f32,
    pub namespace_memory_limits_gb: f32,
    pub namespace_used_cpu: f32,
    pub namespace_used_memory_gb: f32,
}

impl Default for KubernetesMetrics {
    fn default() -> Self {
        Self {
            cluster_total_cpu: 1812.0,  // Default based on real cluster
            cluster_used_cpu: 0.0,
            cluster_total_memory_gb: 2978.0, // 2.91 TiB converted to GB (2.91 * 1024)
            cluster_used_memory_gb: 0.0,
            
            namespace: "larstesting".to_string(),
            namespace_cpu_limits: 256.0,  // Default max CPU for namespace
            namespace_memory_limits_gb: 1024.0, // Default max memory for namespace
            namespace_used_cpu: 0.0,
            namespace_used_memory_gb: 0.0,
        }
    }
}

// Structure to hold Prometheus metrics response
#[derive(Deserialize, Debug, Clone)]
struct PrometheusResponse {
    status: String,
    data: PrometheusData,
}

#[derive(Deserialize, Debug, Clone)]
struct PrometheusData {
    result: Vec<PrometheusResult>,
}

#[derive(Deserialize, Debug, Clone)]
struct PrometheusResult {
    value: (f64, String),  // Timestamp and value
}

// --- Application State ---

#[derive(Default)]
struct SchedulerState {
    mock_completion_timers: HashMap<Uuid, tokio::time::Instant>,
}

#[derive(Clone)]
struct AppState {
    templates: Arc<Mutex<AutoReloader>>,
    queued_simulations: Arc<Mutex<VecDeque<QueuedSimulation>>>,
    active_simulations: Arc<Mutex<HashMap<Uuid, ActiveSimulation>>>,
    last_finished_simulation: Arc<Mutex<Option<LastFinishedSimulation>>>,
    cluster_utilization: Arc<Mutex<ClusterUtilization>>,
    namespace_utilization: Arc<Mutex<NamespaceUtilization>>,
    db_pool: SqlitePool,
    event_sender: broadcast::Sender<AppEvent>,
    scheduler_state: Arc<Mutex<SchedulerState>>, // New field for thread-safe scheduler state
    time_dilation: Arc<Mutex<u32>>, // Time dilation factor
}

// --- Handlers ---

// Debug version of the root handler - remove LiveReload extractor
#[cfg(debug_assertions)]
async fn root_handler_debug(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let reloader = state.templates.lock().await;
    let env = reloader.acquire_env().unwrap();
    let tmpl = env.get_template("index.html.j2").unwrap();
    // The layer injects the script, no need to pass it in context
    let html = tmpl.render(context! {}).unwrap();
    Html(html).into_response()
}

// Release version of the root handler without LiveReload
#[cfg(not(debug_assertions))]
async fn root_handler_release(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let reloader = state.templates.lock().await;
    let env = reloader.acquire_env().unwrap();
    let tmpl = env.get_template("index.html.j2").unwrap();
    // No live_reload_script needed in release builds
    let html = tmpl.render(context! {}).unwrap();
    Html(html).into_response()
}

// SSE Handler
async fn sse_handler(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, axum::Error>>> {
    tracing::info!("SSE client connected");

    // Subscribe to the broadcast channel
    let mut rx = state.event_sender.subscribe();

    // Send the initial state immediately
    let initial_queue_deque = state.queued_simulations.lock().await;
    let initial_queue_vec: Vec<QueuedSimulation> = initial_queue_deque.iter().cloned().collect(); // Convert to Vec
    drop(initial_queue_deque); // Drop lock
    
    let initial_active = state.active_simulations.lock().await.values().cloned().collect();
    
    let initial_last_finished = state.last_finished_simulation.lock().await.clone();
    let initial_cluster_util = state.cluster_utilization.lock().await.clone();
    let initial_namespace_util = state.namespace_utilization.lock().await.clone();

    // Create a vector to hold all initial events
    let mut initial_events = vec![
        Ok(Event::default().json_data(AppEvent::QueueUpdated(initial_queue_vec)).unwrap()),
        Ok(Event::default().json_data(AppEvent::ActiveUpdated(initial_active)).unwrap()),
        Ok(Event::default().json_data(AppEvent::ClusterUtilizationUpdated(initial_cluster_util)).unwrap()),
        Ok(Event::default().json_data(AppEvent::NamespaceUtilizationUpdated(initial_namespace_util)).unwrap()),
    ];
    
    // Add last finished simulation event if available
    if let Some(last_finished) = initial_last_finished {
        initial_events.push(Ok(Event::default().json_data(AppEvent::LastFinished(last_finished)).unwrap()));
    }

    let initial_stream = stream::iter(initial_events);

    // Create the main stream that listens for broadcast updates
    let broadcast_stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(app_event) => {
                    tracing::debug!("Received event via broadcast: {:?}", app_event);
                    // Try to serialize the event to JSON and send it
                    if let Ok(event) = Event::default().json_data(&app_event) {
                         yield Ok(event);
                    } else {
                         tracing::error!("Failed to serialize AppEvent for SSE");
                         // Optionally yield an error event to the client
                         // yield Err(axum::Error::new("Serialization error"));
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::warn!("SSE broadcast channel closed.");
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("SSE client lagged behind by {} messages.", n);
                    // Optionally send a 'resync' event or just continue
                }
            }
        }
    };

    // Combine initial state stream with the broadcast stream
    let stream = initial_stream.chain(broadcast_stream);

    Sse::new(stream).keep_alive(KeepAlive::default())
}

// Mock Submit Handler (Using DB)
#[derive(Deserialize, Debug)]
pub struct MockSubmitRequest {
    count: u32,
    base_nodes: u32,
    // Add more params if needed to control mock generation
}

#[debug_handler]
async fn mock_submit_handler(
    State(state): State<AppState>,
    Json(payload): Json<MockSubmitRequest>,
) -> impl IntoResponse {
    tracing::info!("Received mock submit request: {:?}", payload);
    
    // Create a Send-compatible RNG that can safely cross await points
    let mut rng = StdRng::from_entropy();
    
    // Acquire lock on queue once
    let mut queue = state.queued_simulations.lock().await;
    let db_pool = &state.db_pool;
    
    // Prepare to collect all simulations so we can add them at once
    let mut new_simulations = Vec::with_capacity(payload.count as usize);

    for i in 0..payload.count {
        // Generate all random values using the same RNG instance
        let node_count = payload.base_nodes + rng.gen_range(0..100) * (i + 1);
        let minutes = rng.gen_range(3..=15);
        let duration_secs = minutes * 60; // Convert minutes to seconds
        let chart_num = rng.gen_range(1..4);
        let cpu_per_node = 0.18 + (rng.gen::<f32>() * 0.04 - 0.02);
        let memory_per_node = 0.05 + (rng.gen::<f32>() * 0.02 - 0.01);
        
        let params = SimulationParams {
            chart: format!("mock-chart-{}", chart_num),
            node_count,
            duration_secs,
        };

        // Calculate sanity check cost (using our RNG)
        let sanity_check_cost = ResourceCost {
            cpu_cores: params.node_count as f32 * cpu_per_node,
            memory_gb: params.node_count as f32 * memory_per_node,
        };
        
        tracing::debug!(chart = %params.chart, nodes = %params.node_count, duration = %params.duration_secs, cost = ?sanity_check_cost, "Calculated sanity check cost");
        
        // --- Predict cost using DB lookup with sanity check --- 
        let predicted_cost_result: Result<Option<ResourceCost>, sqlx::Error> = sqlx::query_as(
            "SELECT cpu_cores, memory_gb FROM cost_history WHERE chart = ? AND node_count = ? ORDER BY observed_at DESC LIMIT 1"
        )
        .bind(&params.chart)
        .bind(params.node_count)
        .fetch_optional(db_pool)
        .await;

        let predicted_cost = match predicted_cost_result {
            Ok(Some(historical_cost)) => {
                // Check if historical cost is within a reasonable range (0.5x to 2.0x) of sanity check
                let cpu_ratio = if sanity_check_cost.cpu_cores > 0.0 { historical_cost.cpu_cores / sanity_check_cost.cpu_cores } else { 1.0 };
                let mem_ratio = if sanity_check_cost.memory_gb > 0.0 { historical_cost.memory_gb / sanity_check_cost.memory_gb } else { 1.0 };
                
                // Define acceptable range (allow some variance)
                const MIN_RATIO: f32 = 0.5;
                const MAX_RATIO: f32 = 2.0;
                
                if cpu_ratio >= MIN_RATIO && cpu_ratio <= MAX_RATIO && mem_ratio >= MIN_RATIO && mem_ratio <= MAX_RATIO {
                    tracing::info!(chart = %params.chart, nodes = %params.node_count, cost = ?historical_cost, "Using historical cost from DB (within sanity range)");
                    historical_cost
                } else {
                    tracing::warn!(chart = %params.chart, nodes = %params.node_count, historical = ?historical_cost, sanity = ?sanity_check_cost, "Historical cost out of range ({:.2}x CPU, {:.2}x Mem). Using sanity check cost.", cpu_ratio, mem_ratio);
                    sanity_check_cost // Use sanity check cost if historical is unreasonable
                }
            }
            Ok(None) => {
                tracing::info!(chart = %params.chart, nodes = %params.node_count, cost = ?sanity_check_cost, "No history found, using sanity check cost");
                sanity_check_cost // Use sanity check cost if no history
            }
            Err(e) => {
                tracing::error!(chart = %params.chart, nodes = %params.node_count, "DB error fetching cost: {}. Using sanity check cost.", e);
                sanity_check_cost // Use sanity check cost on DB error
            }
        };

        let queued_sim = QueuedSimulation {
            request_id: Uuid::new_v4(),
            params,
            predicted_cost, // Use the chosen predicted cost
        };

        // Add to our collection instead of directly to queue
        new_simulations.push(queued_sim);
    }
    
    // Now add all simulations to the queue at once
    for sim in new_simulations {
        queue.push_back(sim);
    }

    // Broadcast the queue update
    let queue_updated: Vec<QueuedSimulation> = queue.iter().cloned().collect();
    drop(queue); // Release lock before sending broadcast
    
    // Send after dropping the lock to avoid potential deadlocks
    let _ = state.event_sender.send(AppEvent::QueueUpdated(queue_updated));
    tracing::info!("Added {} mock simulations to queue and broadcasted update", payload.count);

    StatusCode::OK
}

// History Page Handler
#[cfg(debug_assertions)]
async fn history_handler_debug(State(state): State<AppState>) -> impl IntoResponse {
    let reloader = state.templates.lock().await;
    let env = reloader.acquire_env().unwrap();
    let tmpl = env.get_template("history.html.j2").unwrap();
    // The actual history data is loaded via JavaScript from the /api/history endpoint
    Html(tmpl.render(context! {}).unwrap()).into_response()
}

#[cfg(not(debug_assertions))]
async fn history_handler_release(State(state): State<AppState>) -> impl IntoResponse {
    let reloader = state.templates.lock().await;
    let env = reloader.acquire_env().unwrap();
    let tmpl = env.get_template("history.html.j2").unwrap();
    Html(tmpl.render(context! {}).unwrap()).into_response()
}

// API History Handler - Returns JSON data of simulation history
async fn api_history_handler(State(state): State<AppState>) -> impl IntoResponse {
    let db_pool = &state.db_pool;
    
    // Run SQL query to get history from cost_history table
    let result = sqlx::query(
        r#"
        SELECT 
            chart, 
            node_count, 
            duration_secs,
            cpu_cores, 
            memory_gb, 
            observed_at
        FROM cost_history
        ORDER BY observed_at DESC
        LIMIT 100
        "#
    )
    .fetch_all(db_pool)
    .await
    .map(|rows| {
        rows.iter().map(|row| {
            HistoryEntry {
                chart: row.get("chart"),
                node_count: row.get("node_count"),
                duration_secs: row.get("duration_secs"),
                cpu_cores: row.get("cpu_cores"),
                memory_gb: row.get("memory_gb"),
                observed_at: chrono::DateTime::parse_from_rfc3339(&row.get::<String, _>("observed_at"))
                    .unwrap_or_default()
                    .with_timezone(&chrono::Utc),
            }
        }).collect::<Vec<HistoryEntry>>()
    });

    match result {
        Ok(entries) => {
            // Return JSON response
            Json(entries).into_response()
        },
        Err(err) => {
            // Log the error
            tracing::error!("Failed to fetch history data: {}", err);
            // Return empty array with 500 status
            (StatusCode::INTERNAL_SERVER_ERROR, Json(Vec::<HistoryEntry>::new())).into_response()
        }
    }
}

// Fetch metrics from Prometheus
async fn fetch_prometheus_metrics(namespace: &str) -> Result<KubernetesMetrics, Box<dyn std::error::Error + Send + Sync>> {
    let prometheus_url = "https://metrics.riff.cc/select/0/prometheus/api/v1/";
    let http_client = reqwest::Client::new();
    
    // Initialize metrics with default values
    let mut metrics = KubernetesMetrics::default();
    metrics.namespace = namespace.to_string();
    
    // Fetch total cluster CPU capacity
    let cluster_cpu_query = format!("{}query?query={}", prometheus_url, 
        "sum(kube_node_status_capacity{resource=\"cpu\"})");
    
    let response = http_client.get(&cluster_cpu_query).send().await?;
    let prom_response: PrometheusResponse = response.json().await?;
    
    if prom_response.status == "success" && !prom_response.data.result.is_empty() {
        if let Ok(value) = prom_response.data.result[0].value.1.parse::<f32>() {
            metrics.cluster_total_cpu = value;
            tracing::info!("Prometheus: Cluster CPU capacity: {}", value);
        }
    }
    
    // Fetch total cluster memory capacity (in GB)
    let cluster_mem_query = format!("{}query?query={}", prometheus_url, 
        "sum(kube_node_status_capacity{resource=\"memory\"}) / 1024 / 1024 / 1024");
    
    let response = http_client.get(&cluster_mem_query).send().await?;
    let prom_response: PrometheusResponse = response.json().await?;
    
    if prom_response.status == "success" && !prom_response.data.result.is_empty() {
        if let Ok(value) = prom_response.data.result[0].value.1.parse::<f32>() {
            metrics.cluster_total_memory_gb = value;
            tracing::info!("Prometheus: Cluster memory capacity: {} GB", value);
        }
    }
    
    // Fetch cluster CPU usage
    let cluster_cpu_usage_query = format!("{}query?query={}", prometheus_url, 
        "sum(rate(container_cpu_usage_seconds_total[5m]))");
    
    let response = http_client.get(&cluster_cpu_usage_query).send().await?;
    let prom_response: PrometheusResponse = response.json().await?;
    
    if prom_response.status == "success" && !prom_response.data.result.is_empty() {
        if let Ok(value) = prom_response.data.result[0].value.1.parse::<f32>() {
            metrics.cluster_used_cpu = value;
            tracing::info!("Prometheus: Cluster CPU usage: {}", value);
        }
    }
    
    // Fetch cluster memory usage (in GB)
    let cluster_mem_usage_query = format!("{}query?query={}", prometheus_url, 
        "sum(container_memory_working_set_bytes) / 1024 / 1024 / 1024");
    
    let response = http_client.get(&cluster_mem_usage_query).send().await?;
    let prom_response: PrometheusResponse = response.json().await?;
    
    if prom_response.status == "success" && !prom_response.data.result.is_empty() {
        if let Ok(value) = prom_response.data.result[0].value.1.parse::<f32>() {
            metrics.cluster_used_memory_gb = value;
            tracing::info!("Prometheus: Cluster memory usage: {} GB", value);
        }
    }
    
    // Fetch namespace CPU usage
    let namespace_cpu_query = format!("{}query?query={}", prometheus_url, 
        format!("sum(rate(container_cpu_usage_seconds_total{{namespace=\"{}\"}}[5m]))", namespace));
    
    let response = http_client.get(&namespace_cpu_query).send().await?;
    let prom_response: PrometheusResponse = response.json().await?;
    
    if prom_response.status == "success" && !prom_response.data.result.is_empty() {
        if let Ok(value) = prom_response.data.result[0].value.1.parse::<f32>() {
            metrics.namespace_used_cpu = value;
            tracing::info!("Prometheus: Namespace {} CPU usage: {}", namespace, value);
        }
    }
    
    // Fetch namespace memory usage (in GB)
    let namespace_mem_query = format!("{}query?query={}", prometheus_url, 
        format!("sum(container_memory_working_set_bytes{{namespace=\"{}\"}}) / 1024 / 1024 / 1024", namespace));
    
    let response = http_client.get(&namespace_mem_query).send().await?;
    let prom_response: PrometheusResponse = response.json().await?;
    
    if prom_response.status == "success" && !prom_response.data.result.is_empty() {
        if let Ok(value) = prom_response.data.result[0].value.1.parse::<f32>() {
            metrics.namespace_used_memory_gb = value;
            tracing::info!("Prometheus: Namespace {} memory usage: {} GB", namespace, value);
        }
    }
    
    Ok(metrics)
}

// Update the existing function to use Prometheus data
async fn update_utilization_from_k8s(state: AppState) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Specify the simulation namespace
    let namespace = "larstesting";
    
    // Fetch metrics from Prometheus instead of Kubernetes API
    match fetch_prometheus_metrics(namespace).await {
        Ok(metrics) => {
            // Add the active simulations to these base values
            let active_sims = state.active_simulations.lock().await;
            let sim_cpu: f32 = active_sims.values().map(|sim| sim.actual_cost.cpu_cores).sum();
            let sim_memory: f32 = active_sims.values().map(|sim| sim.actual_cost.memory_gb).sum();
            
            // Update cluster utilization - Include simulations in cluster usage
            let mut cluster_util = state.cluster_utilization.lock().await;
            cluster_util.total_cpu_cores = metrics.cluster_total_cpu;
            cluster_util.total_memory_gb = metrics.cluster_total_memory_gb;
            
            // Add simulation resource usage to cluster metrics
            cluster_util.used_cpu_cores = metrics.cluster_used_cpu + sim_cpu;
            cluster_util.used_memory_gb = metrics.cluster_used_memory_gb + sim_memory;
            
            // Recalculate percentages with simulation usage included
            cluster_util.cpu_percent = (cluster_util.used_cpu_cores / cluster_util.total_cpu_cores) * 100.0;
            cluster_util.memory_percent = (cluster_util.used_memory_gb / cluster_util.total_memory_gb) * 100.0;
            
            // Update namespace utilization with the real metrics plus our simulations
            let mut namespace_util = state.namespace_utilization.lock().await;
            
            // Base namespace values from Prometheus
            let base_namespace_cpu = metrics.namespace_used_cpu;
            let base_namespace_memory = metrics.namespace_used_memory_gb;
            
            // Total namespace usage = real metrics + simulation usage
            namespace_util.used_cpu_cores = base_namespace_cpu + sim_cpu;
            namespace_util.used_memory_gb = base_namespace_memory + sim_memory;
            namespace_util.allocated_cpu_cores = metrics.namespace_cpu_limits.max(256.0); // Use at least 256 cores
            namespace_util.allocated_memory_gb = metrics.namespace_memory_limits_gb.max(1024.0); // Use at least 1024 GB
            namespace_util.cpu_percent = (namespace_util.used_cpu_cores / namespace_util.allocated_cpu_cores) * 100.0;
            namespace_util.memory_percent = (namespace_util.used_memory_gb / namespace_util.allocated_memory_gb) * 100.0;
            namespace_util.cluster_total_cpu = metrics.cluster_total_cpu;
            namespace_util.cluster_total_memory = metrics.cluster_total_memory_gb;
            
            // Send utilization updates
            let _ = state.event_sender.send(AppEvent::ClusterUtilizationUpdated(cluster_util.clone()));
            let _ = state.event_sender.send(AppEvent::NamespaceUtilizationUpdated(namespace_util.clone()));
            
            tracing::info!("Updated utilization metrics from Prometheus for namespace '{}'", namespace);
            Ok(())
        },
        Err(e) => {
            tracing::error!("Failed to fetch Prometheus metrics: {}", e);
            Err(e.into())
        }
    }
}

// Adding this between scheduler code and monitoring task:
async fn try_schedule_simulations(state: &AppState, queue: &mut VecDeque<QueuedSimulation>) -> bool {
    // If queue is empty, nothing to do
    if queue.is_empty() {
        return false;
    }

    let _rng = StdRng::from_entropy(); // Changed to _rng to indicate intentionally unused
    let mut active_updated = false;
    
    // Get cluster utilization once at the beginning
    let (current_cpu_percent, used_cpu_cores, total_cpu_cores, used_memory_gb, total_memory_gb) = {
        let cluster_util = state.cluster_utilization.lock().await;
        (
            cluster_util.cpu_percent,
            cluster_util.used_cpu_cores,
            cluster_util.total_cpu_cores,
            cluster_util.used_memory_gb,
            cluster_util.total_memory_gb
        )
    };
    
    // Log current cluster state before scheduling
    tracing::info!(
        "Current cluster state before scheduling - CPU: {:.1}% ({:.1}/{:.1} cores), Memory: {:.1}% ({:.1}/{:.1} GB), Queue size: {}",
        current_cpu_percent,
        used_cpu_cores,
        total_cpu_cores,
        (used_memory_gb / total_memory_gb) * 100.0,
        used_memory_gb,
        total_memory_gb,
        queue.len()
    );
    
    // Process entire queue if possible - no artificial limits
    let items_to_process = queue.len();
    
    // Pre-screen simulations to move to active state
    let mut to_activate = Vec::with_capacity(items_to_process);
    
    // Track cumulative resource impact of all simulations we're considering
    let mut cumulative_cpu_cores = used_cpu_cores;
    let mut cumulative_memory_gb = used_memory_gb;
    
    // First pass: determine which simulations can be moved to active state
    // Track cumulative impact to avoid oversubscription
    for _ in 0..items_to_process {
        if let Some(next_sim) = queue.front() {
            // Always check resource constraints for every simulation
            let predicted_cpu = next_sim.predicted_cost.cpu_cores;
            let predicted_memory = next_sim.predicted_cost.memory_gb;
            
            // Calculate what would happen if we admit this simulation
            let new_total_cpu = cumulative_cpu_cores + predicted_cpu;
            let new_total_memory = cumulative_memory_gb + predicted_memory;
            
            // Calculate new percentages
            let new_cpu_percent = (new_total_cpu / total_cpu_cores) * 100.0;
            let new_memory_percent = (new_total_memory / total_memory_gb) * 100.0;
            
            // Make admission decision based on projected resource usage
            // Lower thresholds to be more conservative
            let cpu_ok = new_cpu_percent <= 80.0; // Reduced to 80%
            let memory_ok = new_memory_percent <= 85.0; // Reduced from 95% to 85%
            
            if !cpu_ok {
                tracing::info!(
                    "Admitting next simulation would exceed CPU capacity ({:.1}% -> {:.1}%), pausing admission", 
                    (cumulative_cpu_cores / total_cpu_cores) * 100.0,
                    new_cpu_percent
                );
                break;
            }
            
            if !memory_ok {
                tracing::info!(
                    "Admitting next simulation would exceed memory capacity ({:.1}% -> {:.1}%), pausing admission", 
                    (cumulative_memory_gb / total_memory_gb) * 100.0,
                    new_memory_percent
                );
                break;
            }
            
            // This simulation can be admitted, pop it from queue
            if let Some(sim) = queue.pop_front() {
                // Update our cumulative tracking of resource usage
                cumulative_cpu_cores += predicted_cpu;
                cumulative_memory_gb += predicted_memory;
                
                // Add to activation list
                to_activate.push(sim);
            }
        } else {
            break; // Queue is empty
        }
    }
    
    // If we're activating simulations, log the projected impact
    if !to_activate.is_empty() {
        let final_cpu_percent = (cumulative_cpu_cores / total_cpu_cores) * 100.0;
        let final_memory_percent = (cumulative_memory_gb / total_memory_gb) * 100.0;
        
        tracing::info!(
            "Moving {} simulations from queue to active. Projected impact: CPU {:.1}% -> {:.1}%, Memory {:.1}% -> {:.1}%",
            to_activate.len(),
            current_cpu_percent,
            final_cpu_percent,
            (used_memory_gb / total_memory_gb) * 100.0,
            final_memory_percent
        );
        
        // Second pass: actually move simulations to active state
        for queued_sim in to_activate {
            let sim_id = Uuid::new_v4();
            
            // Create active simulation record
            let active_sim = ActiveSimulation {
                simulation_id: sim_id,
                params: queued_sim.params.clone(),
                predicted_cost: queued_sim.predicted_cost.clone(),
                actual_cost: queued_sim.predicted_cost.clone(),
                usage_snapshots: Vec::new(),
                last_snapshot_time: chrono::Utc::now(),
            };
            
            // Add to active simulations
            state.active_simulations.lock().await.insert(sim_id, active_sim.clone());
            
            // Add to completion timers
            {
                let mut sched_state = state.scheduler_state.lock().await;
                sched_state.mock_completion_timers.insert(sim_id, tokio::time::Instant::now());
            }
            
            tracing::debug!(
                simulation_id = %sim_id, 
                chart = %queued_sim.params.chart,
                nodes = %queued_sim.params.node_count,
                duration = %queued_sim.params.duration_secs,
                cpu = %queued_sim.predicted_cost.cpu_cores,
                memory = %queued_sim.predicted_cost.memory_gb,
                "Simulation moved from queue to active state"
            );
            
            active_updated = true;
        }
    } else if !queue.is_empty() {
        // If we couldn't schedule any simulations but there are items in the queue
        tracing::info!(
            "Not scheduling any simulations from queue due to resource constraints. Current usage: CPU {:.1}%, Memory {:.1}%",
            current_cpu_percent,
            (used_memory_gb / total_memory_gb) * 100.0
        );
    }
    
    active_updated
}

// Add the time dilation request struct
#[derive(Deserialize)]
struct TimeDilationRequest {
    factor: u32,
}

// Add handler for setting time dilation
async fn set_time_dilation_handler(
    State(state): State<AppState>,
    Json(payload): Json<TimeDilationRequest>,
) -> impl IntoResponse {
    let mut time_dilation = state.time_dilation.lock().await;
    
    // Validate and apply the time dilation factor
    let factor = match payload.factor {
        1 | 3 | 5 | 10 => payload.factor, // Only allow 1x, 3x, 5x, or 10x
        _ => 1, // Default to 1x for invalid values
    };
    
    *time_dilation = factor;
    tracing::info!("Time dilation set to {}x", factor);
    
    StatusCode::OK
}

// --- Main Function ---

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenv().ok();
    // Initialize tracing (logging)
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lars=debug,tower_http=debug,minijinja=warn".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    tracing::info!("Initializing LARS...");

    // Setup MiniJinja template environment
    let templates_reloader = AutoReloader::new(|notifier| {
        let mut env = Environment::new();
        // Load templates from the 'templates' directory
        env.set_loader(path_loader("templates"));
        // Watch the templates directory for changes
        notifier.watch_path("templates", true);
        Ok(env)
    });

    // --- Database Setup ---
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");

    // Create database if it doesn't exist
    if !Sqlite::database_exists(&db_url).await.unwrap_or(false) {
        tracing::info!("Creating database {}", db_url);
        Sqlite::create_database(&db_url).await?;
    } else {
        tracing::info!("Database already exists.");
    }

    // Create connection pool
    let db_pool = SqlitePool::connect(&db_url).await.expect("Failed to connect to database");
    tracing::info!("Database connection pool established.");

    // Run migrations
    sqlx::migrate!("./migrations")
        .run(&db_pool)
        .await
        .expect("Failed to run database migrations");
    tracing::info!("Database migrations applied successfully.");

    // --- Initial State Setup ---
    let (event_sender, _) = broadcast::channel::<AppEvent>(100);

    let state = AppState {
        templates: Arc::new(Mutex::new(templates_reloader)),
        queued_simulations: Arc::new(Mutex::new(VecDeque::new())),
        active_simulations: Arc::new(Mutex::new(HashMap::new())),
        last_finished_simulation: Arc::new(Mutex::new(None)),
        cluster_utilization: Arc::new(Mutex::new(ClusterUtilization::default())),
        namespace_utilization: Arc::new(Mutex::new(NamespaceUtilization::default())),
        db_pool,
        event_sender,
        scheduler_state: Arc::new(Mutex::new(SchedulerState { mock_completion_timers: HashMap::new() })),
        time_dilation: Arc::new(Mutex::new(1)), // Default time dilation factor
    };

    // --- Mock Scheduler Task (Using DB) ---
    let scheduler_state = state.clone();
    tokio::spawn(async move {
        // Increase frequency for faster processing when we have lots of simulations
        let mut interval = tokio::time::interval(Duration::from_secs(5)); 
        
        loop {
            interval.tick().await;

            // Initialize tracking variables
            let mut queue_updated = false;
            let mut active_updated = false;
            
            // Get the current queue state
            let mut queue = scheduler_state.queued_simulations.lock().await;
            let initial_queue_size = queue.len();
            let db_pool = &scheduler_state.db_pool;
            
            tracing::debug!("Scheduler tick - current queue size: {}", initial_queue_size);

            // Process completions in batches to handle thousands of active simulations efficiently
            let time_dilation = *scheduler_state.time_dilation.lock().await;
            let now = tokio::time::Instant::now();
            
            // 1. Get all completion candidates in one pass
            let mut completed_ids = Vec::new();
            let mut completed_sim_data = Vec::new();
            
            {
                // Scan all active simulations for completions
                let sched_state = scheduler_state.scheduler_state.lock().await;
                let active_sims = scheduler_state.active_simulations.lock().await;
                
                // Pre-allocate a large enough vector when we have many active simulations
                completed_ids.reserve(active_sims.len() / 4); // Assume ~25% might be complete
                
                for (id, start_time) in &sched_state.mock_completion_timers {
                    if let Some(sim) = active_sims.get(id) {
                        // Apply time dilation to make simulations run faster
                        let duration_with_dilation = Duration::from_secs(sim.params.duration_secs as u64 / time_dilation as u64);
                        if now.duration_since(*start_time) >= duration_with_dilation {
                            completed_ids.push(*id);
                        }
                    }
                }
                
                tracing::debug!("Found {} simulations to complete out of {} active", 
                    completed_ids.len(), active_sims.len());
            }

            // 2. Process the completions
            if !completed_ids.is_empty() {
                // Get write access to process completions
                let mut active_sims = scheduler_state.active_simulations.lock().await;
                let mut sched_state = scheduler_state.scheduler_state.lock().await;
                
                // Reserve space for completed simulation data
                completed_sim_data.reserve(completed_ids.len());
                
                for id in &completed_ids {
                    if let Some(completed_sim) = active_sims.remove(id) {
                        sched_state.mock_completion_timers.remove(id);
                        active_updated = true;
                        
                        // Update last finished simulation
                        let last_finished = LastFinishedSimulation {
                            simulation_id: *id,
                            params: completed_sim.params.clone(),
                            predicted_cost: completed_sim.predicted_cost.clone(),
                            actual_cost: completed_sim.actual_cost.clone(),
                            finished_at: chrono::Utc::now(),
                            duration_secs: completed_sim.params.duration_secs,
                        };
                        
                        // Update last finished simulation in state
                        *scheduler_state.last_finished_simulation.lock().await = Some(last_finished.clone());
                        
                        // Broadcast last finished simulation update
                        let _ = scheduler_state.event_sender.send(AppEvent::LastFinished(last_finished));
                        
                        // Collect data needed for DB insert
                        completed_sim_data.push((completed_sim.params.clone(), completed_sim.actual_cost.clone()));
                    }
                }
                
                tracing::info!("Processed {} completed simulations", completed_ids.len());
                
                // Log detailed stats for select completed simulations when handling many
                if completed_ids.len() > 10 {
                    tracing::info!("Large batch completion: processed {} simulations", completed_ids.len());
                } else {
                    // Log detailed stats for individual simulations when we have fewer
                    for (i, (params, cost)) in completed_sim_data.iter().enumerate().take(5) {
                        tracing::info!(
                            index = i,
                            chart = %params.chart,
                            nodes = %params.node_count,
                            duration = %params.duration_secs,
                            cpu = %cost.cpu_cores,
                            memory = %cost.memory_gb,
                            "Simulation completed"
                        );
                    }
                }
            }

            // 3. Store data in the database - batch insert if possible
            if !completed_sim_data.is_empty() {
                // Add all completed simulations to the DB
                for (params, cost) in &completed_sim_data {
                    let query_result = sqlx::query(
                        "INSERT OR REPLACE INTO cost_history (chart, node_count, duration_secs, cpu_cores, memory_gb, observed_at) VALUES (?, ?, ?, ?, ?, datetime('now'))"
                    )
                    .bind(&params.chart)
                    .bind(params.node_count)
                    .bind(params.duration_secs)
                    .bind(cost.cpu_cores)
                    .bind(cost.memory_gb)
                    .execute(db_pool)
                    .await;

                    if let Err(e) = query_result {
                        tracing::error!(chart = %params.chart, nodes = %params.node_count, "Failed to store cost in DB: {}", e);
                    }
                }
                
                tracing::info!("Stored {} simulation records in the database", completed_sim_data.len());
            }

            // 4. Schedule new simulations from queue
            if !queue.is_empty() {
                if try_schedule_simulations(&scheduler_state, &mut queue).await {
                    active_updated = true;
                    
                    if queue.len() != initial_queue_size {
                        queue_updated = true;
                        tracing::info!("Queue size changed from {} to {} after scheduling", 
                             initial_queue_size, queue.len());
                    }
                }
            }

            // 5. Broadcast updates efficiently
            if queue_updated {
                let current_queue: Vec<QueuedSimulation> = queue.iter().cloned().collect();
                drop(queue); // Release lock before broadcasting
                
                if let Err(e) = scheduler_state.event_sender.send(AppEvent::QueueUpdated(current_queue)) {
                    tracing::error!("Failed to broadcast queue update: {}", e);
                }
            } else {
                drop(queue); // Release lock if we didn't use it for broadcasting
            }
            
            if active_updated {
                let current_active = scheduler_state.active_simulations.lock().await.values().cloned().collect();
                
                if let Err(e) = scheduler_state.event_sender.send(AppEvent::ActiveUpdated(current_active)) {
                    tracing::error!("Failed to broadcast active simulations update: {}", e);
                }
            }
        }
    });

    // --- Mock Monitoring Task ---
    let monitor_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5)); // Update every 5 seconds
        let snapshot_interval = Duration::from_secs(30); // Take snapshots every 30 seconds
        
        loop {
            interval.tick().await;
            
            // Update actual costs for all active simulations first
            {
                let mut active_sims = monitor_state.active_simulations.lock().await;
                let _cluster_util = monitor_state.cluster_utilization.lock().await; // Prefix with _ to indicate intentionally unused
                let mut updated = false;
                let now = chrono::Utc::now();
                
                for sim in active_sims.values_mut() {
                    // Create a Send-compatible RNG for each simulation
                    let mut local_rng = StdRng::from_entropy();
                    
                    // Apply realistic variation with jitter that only increases resource usage
                    let cpu_jitter = 1.0 + local_rng.gen_range(0.0..=0.2); // 1.0-1.2x (only increase)
                    let mem_jitter = 1.0 + local_rng.gen_range(0.0..=0.15); // 1.0-1.15x (only increase)
                    
                    // Calculate actual cost based on predicted cost + jitter
                    let current_cpu_cores = sim.predicted_cost.cpu_cores * cpu_jitter;
                    let current_memory_gb = sim.predicted_cost.memory_gb * mem_jitter;
                    
                    // Update current actual cost
                    sim.actual_cost.cpu_cores = current_cpu_cores;
                    sim.actual_cost.memory_gb = current_memory_gb;
                    
                    // Check if it's time to take a snapshot (every 30 seconds)
                    let time_since_last = now.signed_duration_since(sim.last_snapshot_time);
                    if time_since_last.num_seconds() >= snapshot_interval.as_secs() as i64 {
                        // Take a snapshot of current resource usage
                        sim.usage_snapshots.push(ResourceSnapshot {
                            timestamp: now,
                            cpu_cores: current_cpu_cores,
                            memory_gb: current_memory_gb,
                        });
                        
                        // Update last snapshot time
                        sim.last_snapshot_time = now;
                        
                        tracing::debug!(
                            simulation_id = %sim.simulation_id,
                            snapshot_count = sim.usage_snapshots.len(),
                            "Resource snapshot taken: CPU={:.2} cores, Memory={:.2} GB",
                            current_cpu_cores, current_memory_gb
                        );
                    }
                    
                    updated = true;
                }
                
                // If there were updates, broadcast them
                if updated && !active_sims.is_empty() {
                    let active_list: Vec<ActiveSimulation> = active_sims.values().cloned().collect();
                    let _ = monitor_state.event_sender.send(AppEvent::ActiveUpdated(active_list));
                }
            }
            
            // Try to get real metrics from Prometheus
            if let Err(e) = update_utilization_from_k8s(monitor_state.clone()).await {
                tracing::warn!("Failed to fetch Prometheus metrics: {:?}", e);
                
                // If Prometheus fetch fails, fall back to calculating metrics from active simulations
                let active_sims = monitor_state.active_simulations.lock().await;
                
                if active_sims.len() > 0 {
                    // Calculate total namespace usage from active simulations
                    let sim_cpu: f32 = active_sims.values().map(|sim| sim.actual_cost.cpu_cores).sum();
                    let sim_memory: f32 = active_sims.values().map(|sim| sim.actual_cost.memory_gb).sum();
                    
                    tracing::debug!("Active simulations resource usage: CPU {:.2} cores, Memory {:.2} GB", sim_cpu, sim_memory);
                    
                    // Update namespace utilization with simulation data
                    let mut namespace_util = monitor_state.namespace_utilization.lock().await;
                    namespace_util.used_cpu_cores = sim_cpu;
                    namespace_util.used_memory_gb = sim_memory;
                    namespace_util.cpu_percent = (sim_cpu / namespace_util.allocated_cpu_cores) * 100.0;
                    namespace_util.memory_percent = (sim_memory / namespace_util.allocated_memory_gb) * 100.0;
                    
                    // Get current cluster state
                    let mut cluster_util = monitor_state.cluster_utilization.lock().await;
                    
                    // Update namespace with cluster totals
                    namespace_util.cluster_total_cpu = cluster_util.total_cpu_cores;
                    namespace_util.cluster_total_memory = cluster_util.total_memory_gb;
                    
                    // Update cluster metrics using our simulations plus base load
                    // This is only a fallback if Prometheus fails
                    let base_cluster_cpu = cluster_util.total_cpu_cores * 0.25; // 25% base load
                    let base_cluster_memory = cluster_util.total_memory_gb * 0.3; // 30% base load
                    
                    cluster_util.used_cpu_cores = sim_cpu + base_cluster_cpu;
                    cluster_util.used_memory_gb = sim_memory + base_cluster_memory;
                    cluster_util.cpu_percent = (cluster_util.used_cpu_cores / cluster_util.total_cpu_cores) * 100.0;
                    cluster_util.memory_percent = (cluster_util.used_memory_gb / cluster_util.total_memory_gb) * 100.0;
                    
                    // Send updates
                    let _ = monitor_state.event_sender.send(AppEvent::NamespaceUtilizationUpdated(namespace_util.clone()));
                    let _ = monitor_state.event_sender.send(AppEvent::ClusterUtilizationUpdated(cluster_util.clone()));
                    
                    tracing::debug!(
                        "Resource update (simulations only) - Namespace: CPU {:.1} cores, Memory {:.1} GB", 
                        sim_cpu, sim_memory
                    );
                }
            }
        }
    });

    // Build our application router
    let mut app = Router::new()
        // Conditionally register the correct root handler
        .route("/",
            #[cfg(debug_assertions)]
            get(root_handler_debug),
            #[cfg(not(debug_assertions))]
            get(root_handler_release)
        )
        // --- API Routes ---
        .route("/api/history", get(api_history_handler))
        .route("/status-stream", get(sse_handler))
        // --- Mocking Routes ---
        .route("/mock_submit", post(mock_submit_handler))
        // --- Time Dilation Route ---
        .route("/set_time_dilation", post(set_time_dilation_handler))
        // --- Static Files & State ---
        .nest_service("/static", ServeDir::new("static"))
        // Add history page route
        .route("/history",
            #[cfg(debug_assertions)]
            get(history_handler_debug),
            #[cfg(not(debug_assertions))]
            get(history_handler_release)
        )
        .with_state(state.clone())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        );

    // --- Live Reload Layer (only added in debug) ---
    #[cfg(debug_assertions)]
    {
        tracing::info!("Enabling live reload layer");
        app = app.layer(LiveReloadLayer::new());
    }

    // Define the server address
    let addr = SocketAddr::from(([0, 0, 0, 0], 9930));
    tracing::info!("Listening on {}", addr);

    // Run the server
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app.into_make_service())
        .await?;

    Ok(())
}

// TODO: Add handlers for /request_run, /simulation_complete, /status-stream
// TODO: Implement Scheduler Core logic
// TODO: Implement Monitoring Module (Kubernetes, Prometheus)
// TODO: Implement Cost Database (SQLite)
// TODO: Implement State Manager (Queues, Active Sims)
// TODO: Implement mock scheduler logic to move from queue to active
// TODO: Implement request_run_handler
// TODO: Implement simulation_complete_handler
// TODO: Implement Cost Database (mock version)
// TODO: Update scheduler task to write completion cost to DB
// TODO: Update mock_submit_handler to read predicted cost from DB
// TODO: Add SQL migration file
