use axum::{
    routing::{get, post},
    Router,
    response::{Html, IntoResponse, sse::{Event, Sse, KeepAlive}},
    extract::{State, Json, Path},
};
use minijinja::{path_loader, Environment, context};
use minijinja_autoreload::AutoReloader;
use std::{net::SocketAddr, sync::Arc, collections::HashMap, time::Duration, collections::VecDeque};
use tokio::sync::{Mutex, broadcast};
use tower_http::{
    services::ServeDir,
    trace::{DefaultMakeSpan, TraceLayer},
};
#[cfg(feature = "debug")]
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
use urlencoding;
use base64::Engine;

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

// --- Cost Constants (Example € per hour) ---
const COST_PER_CPU_CORE_HOUR: f32 = 0.01;
const COST_PER_MEMORY_GB_HOUR: f32 = 0.005;

// Helper function to calculate monetary cost
fn calculate_monetary_cost(cost: &ResourceCost, duration_secs: u32) -> f32 {
    let duration_hours = duration_secs as f32 / 3600.0;
    let cpu_cost = cost.cpu_cores * COST_PER_CPU_CORE_HOUR * duration_hours;
    let memory_cost = cost.memory_gb * COST_PER_MEMORY_GB_HOUR * duration_hours;
    cpu_cost + memory_cost
}

#[derive(Serialize, Deserialize, Debug, Clone, FromRow)]
pub struct ResourceCost {
    #[sqlx(rename = "cpu_cores")]
    pub cpu_cores: f32,
    #[sqlx(rename = "memory_gb")]
    pub memory_gb: f32,
    // Add monetary cost, calculated on demand
    #[serde(skip_serializing_if = "Option::is_none")]
    #[sqlx(default)]
    pub monetary_cost_eur: Option<f32>,
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
            monetary_cost_eur: None,
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
    pub issue_number: Option<u64>, // Added to track origin
    // Add other parameters needed directly from the issue
    pub docker_image: String,
    pub pubsub_topic: String, // Waku specific
    pub bootstrap_nodes: Option<u32>, // Waku specific (ADDED)
    pub publisher_enabled: bool, // Waku specific
    pub publisher_message_size: u32, // Waku specific
    pub publisher_delay: u32, // Waku specific
    pub publisher_message_count: u32, // Waku specific
    pub artificial_latency: bool, // Waku specific
    pub latency_ms: u32, // Waku specific
    pub nodes_command: Option<String>, // Waku specific
    pub bootstrap_command: Option<String>, // Waku specific
    // Nim-libp2p specific
    pub peer_number: Option<u32>,
    pub number_of_peers: Option<u32>,
    pub peers_to_connect: Option<u32>,
    pub message_rate: Option<u32>,
    pub message_size: Option<u32>,
    // Common
    pub parallel_limit: u32,
}

// Define a new struct for resource usage snapshots
#[derive(Serialize, Clone, Debug, serde::Deserialize)] // Added Deserialize
pub struct ResourceSnapshot {
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub cpu_cores: f32,
    pub memory_gb: f32,
}

#[derive(Serialize, Clone, Debug, serde::Deserialize)] // Added Deserialize
pub struct ActiveSimulation {
    pub simulation_id: Uuid,       // ID of this specific run
    pub request_id: Uuid,          // ID from the original QueuedSimulation
    pub params: SimulationParams,  // Basic params (chart, nodes, duration)
    pub predicted_cost: ResourceCost,
    pub actual_cost: ResourceCost, // Will be updated by monitoring
    pub usage_snapshots: Vec<ResourceSnapshot>,
    pub last_snapshot_time: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monetary_cost_eur_predicted: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monetary_cost_eur_actual: Option<f32>,
    pub release_name: String,    // Generated Helm release name
    pub issue_number: Option<u64>, // Original GitHub Issue number
    pub status: String,            // e.g., "Deploying", "Running", "CleaningUp", "Failed", "Completed"
    pub start_time: Option<chrono::DateTime<chrono::Utc>>, // Time deployment started
    pub end_time: Option<chrono::DateTime<chrono::Utc>>,     // Time simulation finished

    // Add detailed parameters needed for deployment
    pub docker_image: String,
    pub pubsub_topic: String, // Waku specific
    pub bootstrap_nodes: u32, // Waku specific (already in SimulationParams? No, add here)
    pub publisher_enabled: bool, // Waku specific
    pub publisher_message_size: u32, // Waku specific
    pub publisher_delay: u32, // Waku specific
    pub publisher_message_count: u32, // Waku specific
    pub artificial_latency: bool, // Waku specific
    pub latency_ms: u32, // Waku specific
    pub nodes_command: Option<String>, // Waku specific
    pub bootstrap_command: Option<String>, // Waku specific
    // Nim-libp2p specific
    pub peer_number: Option<u32>,
    pub number_of_peers: Option<u32>,
    pub peers_to_connect: Option<u32>,
    pub message_rate: Option<u32>,
    pub message_size: Option<u32>,
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

// --- Payload Structs for External Integration --- 

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RequestRunPayload {
    chart: String,
    node_count: u32,
    duration_secs: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RequestRunResponse {
    status: String, // "ACCEPTED" or "REJECTED"
    simulation_id: Option<Uuid>, // ID assigned by LARS if accepted
    reason: Option<String>, // Optional reason for rejection
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReportStartPayload {
    simulation_id: Uuid, 
    chart: String,
    node_count: u32,
    duration_secs: u32,
    release_name: String, // Add release_name to track the deployment
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReportCompletePayload {
    simulation_id: Uuid,
    // Optional final costs from the external runner
    final_cpu_cores: Option<f32>,
    final_memory_gb: Option<f32>,
}

// Mock Submit Handler Request Struct
#[derive(Deserialize, Debug)]
pub struct MockSubmitRequest {
    count: u32,
    base_nodes: u32,
}

// Time Dilation Handler Request Struct
#[derive(Deserialize)]
struct TimeDilationRequest {
    factor: u32,
}

// --- Application State ---

#[derive(Default)]
struct SchedulerState { // Keep for potential future internal tasks, but remove timer map
    // mock_completion_timers: HashMap<Uuid, tokio::time::Instant>,
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
    scheduler_state: Arc<Mutex<SchedulerState>>,
    time_dilation: Arc<Mutex<u32>>,
    pending_simulations: Arc<Mutex<HashMap<Uuid, ResourceCost>>>,
    // Add GitHub config
    github_token: String,
    github_repo: String,
    github_authorized_users: Vec<String>, // Add authorized users
    http_client: reqwest::Client, // Add shared reqwest client
}

// --- Handlers (defined OUTSIDE main) ---

// Debug version of the root handler 
#[cfg(debug_assertions)]
async fn root_handler_debug(
    State(state): State<AppState>,
) -> impl IntoResponse {
    // Get the templates reloader
    let mut reloader = state.templates.lock().await;
    
    // Render the template with proper error handling
    let result = (|| {
        let env = reloader.acquire_env()?;
        let tmpl = env.get_template("index.html.j2")?;
        let html = tmpl.render(context! {})?;
        Ok::<_, minijinja::Error>(html)
    })();
    
    // Handle the result
    match result {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("Template rendering error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template rendering error").into_response()
        }
    }
}

// Release version of the root handler
#[cfg(not(debug_assertions))]
async fn root_handler_release(
    State(state): State<AppState>,
) -> impl IntoResponse {
    // Get the templates reloader
    let mut reloader = state.templates.lock().await;
    
    // Render the template with proper error handling
    let result = (|| {
        let env = reloader.acquire_env()?;
        let tmpl = env.get_template("index.html.j2")?;
        let html = tmpl.render(context! {})?;
        Ok::<_, minijinja::Error>(html)
    })();
    
    // Handle the result
    match result {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("Template rendering error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template rendering error").into_response()
        }
    }
}

// History Page Handlers
#[cfg(debug_assertions)]
async fn history_handler_debug(State(state): State<AppState>) -> impl IntoResponse {
    // Get the templates reloader
    let mut reloader = state.templates.lock().await;
    
    // Render the template with proper error handling
    let result = (|| {
        let env = reloader.acquire_env()?;
        let tmpl = env.get_template("history.html.j2")?;
        let html = tmpl.render(context! {})?;
        Ok::<_, minijinja::Error>(html)
    })();
    
    // Handle the result
    match result {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("Template rendering error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template rendering error").into_response()
        }
    }
}

#[cfg(not(debug_assertions))]
async fn history_handler_release(State(state): State<AppState>) -> impl IntoResponse {
    // Get the templates reloader
    let mut reloader = state.templates.lock().await;
    
    // Render the template with proper error handling
    let result = (|| {
        let env = reloader.acquire_env()?;
        let tmpl = env.get_template("history.html.j2")?;
        let html = tmpl.render(context! {})?;
        Ok::<_, minijinja::Error>(html)
    })();
    
    // Handle the result
    match result {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            tracing::error!("Template rendering error: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Template rendering error").into_response()
        }
    }
}

// --- External API Handlers ---

#[debug_handler]
async fn request_run_handler(
    State(state): State<AppState>,
    Json(payload): Json<RequestRunPayload>,
) -> impl IntoResponse {
    tracing::info!("Received external run request: {:?}", payload);
    let db_pool = &state.db_pool;

    // --- LARS Cost Prediction --- 
    let params = SimulationParams {
        chart: payload.chart.clone(),
        node_count: payload.node_count,
        duration_secs: payload.duration_secs,
    };
    let requested_nodes = params.node_count as f32;
    
    // Basic sanity check prediction (fallback if no history at all)
    let default_cpu_per_node = if params.chart == "waku" { 0.1 } else { 0.08 };
    let default_mem_per_node = if params.chart == "waku" { 0.05 } else { 0.04 };
    let sanity_check_cost = ResourceCost {
        cpu_cores: requested_nodes * default_cpu_per_node,
        memory_gb: requested_nodes * default_mem_per_node,
        monetary_cost_eur: None, 
    };

    // Fetch exact match, closest lower, and closest higher historical costs
    // Use Common Table Expressions (CTEs) for clarity
    let query = r#"
        WITH ExactMatch AS (
            SELECT cpu_cores, memory_gb, node_count 
            FROM cost_history 
            WHERE chart = ? AND node_count = ? 
            ORDER BY observed_at DESC LIMIT 1
        ),
        ClosestLower AS (
            SELECT cpu_cores, memory_gb, node_count 
            FROM cost_history 
            WHERE chart = ? AND node_count < ? 
            ORDER BY node_count DESC LIMIT 1
        ),
        ClosestHigher AS (
            SELECT cpu_cores, memory_gb, node_count 
            FROM cost_history 
            WHERE chart = ? AND node_count > ? 
            ORDER BY node_count ASC LIMIT 1
        )
        SELECT 
            em.cpu_cores AS em_cpu, em.memory_gb AS em_mem, em.node_count AS em_nodes,
            cl.cpu_cores AS cl_cpu, cl.memory_gb AS cl_mem, cl.node_count AS cl_nodes,
            ch.cpu_cores AS ch_cpu, ch.memory_gb AS ch_mem, ch.node_count AS ch_nodes
        FROM 
            (SELECT 1) dummy -- Ensure we always get one row
        LEFT JOIN ExactMatch em ON 1=1
        LEFT JOIN ClosestLower cl ON 1=1
        LEFT JOIN ClosestHigher ch ON 1=1;
    "#;

    let historical_data_result: Result<Option<(Option<f32>, Option<f32>, Option<i64>, Option<f32>, Option<f32>, Option<i64>, Option<f32>, Option<f32>, Option<i64>)>, sqlx::Error> = 
        sqlx::query_as(query)
        .bind(&params.chart)
        .bind(params.node_count)
        .bind(&params.chart)
        .bind(params.node_count)
        .bind(&params.chart)
        .bind(params.node_count)
        .fetch_optional(db_pool)
        .await;

    // Determine predicted cost based on fetched data
    let predicted_cost = match historical_data_result {
        Ok(Some((em_cpu, em_mem, _em_nodes, cl_cpu, cl_mem, cl_nodes, ch_cpu, ch_mem, ch_nodes))) => {
            if let (Some(cpu), Some(mem)) = (em_cpu, em_mem) {
                // Exact match found - use it
                tracing::info!(chart = %params.chart, nodes = %params.node_count, "Using exact historical match for prediction");
                ResourceCost { cpu_cores: cpu, memory_gb: mem, monetary_cost_eur: None }
            } else {
                // No exact match, try scaling from closest neighbor
                let lower_diff = cl_nodes.map(|n| (params.node_count as i64).abs_diff(n));
                let higher_diff = ch_nodes.map(|n| (params.node_count as i64).abs_diff(n));

                // Choose the closest neighbor (prefer lower if equidistant)
                match (lower_diff, higher_diff) {
                    (Some(ld), Some(hd)) if ld <= hd => {
                        // Scale from lower
                        let (hist_cpu, hist_mem, hist_nodes) = (cl_cpu.unwrap(), cl_mem.unwrap(), cl_nodes.unwrap() as f32);
                        let scaled_cpu = hist_cpu * (requested_nodes / hist_nodes);
                        let scaled_mem = hist_mem * (requested_nodes / hist_nodes);
                        tracing::info!(chart = %params.chart, nodes = %params.node_count, from_nodes = %cl_nodes.unwrap(), "Linearly scaling prediction from closest lower history");
                        ResourceCost { cpu_cores: scaled_cpu, memory_gb: scaled_mem, monetary_cost_eur: None }
                    },
                    (Some(_), Some(_)) | (None, Some(_))=> {
                         // Scale from higher (either higher is closer or only higher exists)
                        let (hist_cpu, hist_mem, hist_nodes) = (ch_cpu.unwrap(), ch_mem.unwrap(), ch_nodes.unwrap() as f32);
                        let scaled_cpu = hist_cpu * (requested_nodes / hist_nodes);
                        let scaled_mem = hist_mem * (requested_nodes / hist_nodes);
                        tracing::info!(chart = %params.chart, nodes = %params.node_count, from_nodes = %ch_nodes.unwrap(), "Linearly scaling prediction from closest higher history");
                        ResourceCost { cpu_cores: scaled_cpu, memory_gb: scaled_mem, monetary_cost_eur: None }
                    },
                     (Some(_), None) => {
                        // Only lower exists, scale from it
                        let (hist_cpu, hist_mem, hist_nodes) = (cl_cpu.unwrap(), cl_mem.unwrap(), cl_nodes.unwrap() as f32);
                        let scaled_cpu = hist_cpu * (requested_nodes / hist_nodes);
                        let scaled_mem = hist_mem * (requested_nodes / hist_nodes);
                        tracing::info!(chart = %params.chart, nodes = %params.node_count, from_nodes = %cl_nodes.unwrap(), "Linearly scaling prediction from only available lower history");
                        ResourceCost { cpu_cores: scaled_cpu, memory_gb: scaled_mem, monetary_cost_eur: None }
                    }
                    (None, None) => {
                        // No history for this chart at all - use sanity check
                        tracing::warn!(chart = %params.chart, nodes = %params.node_count, "No historical data found for chart. Using default sanity check prediction.");
                        sanity_check_cost
                    }
                }
            }
        },
        Ok(None) => {
             // Should not happen with the dummy SELECT, but handle anyway: No history for this chart - use sanity check
            tracing::warn!(chart = %params.chart, nodes = %params.node_count, "No historical data found for chart (query returned None). Using default sanity check prediction.");
            sanity_check_cost
        },
        Err(e) => {
            // DB Error - use sanity check
            tracing::error!(chart = %params.chart, nodes = %params.node_count, "DB error fetching cost history for prediction: {}. Using default sanity check prediction.", e);
            sanity_check_cost
        }
    };
    // --- End LARS Cost Prediction --- 

    // 2. Check Admission Control (using LARS predicted_cost)
    let (can_admit, reason) = {
        let cluster_util = state.cluster_utilization.lock().await;
        let predicted_cpu = predicted_cost.cpu_cores;
        let predicted_memory = predicted_cost.memory_gb;
        
        let new_total_cpu = cluster_util.used_cpu_cores + predicted_cpu;
        let new_total_memory = cluster_util.used_memory_gb + predicted_memory;
        
        let new_cpu_percent = (new_total_cpu / cluster_util.total_cpu_cores) * 100.0;
        let new_memory_percent = (new_total_memory / cluster_util.total_memory_gb) * 100.0;
        
        let cpu_ok = new_cpu_percent <= 30.0; 
        let memory_ok = new_memory_percent <= 85.0;
        
        if cpu_ok && memory_ok {
            (true, None)
        } else {
            let mut reasons = Vec::new();
            if !cpu_ok { reasons.push(format!("CPU limit ({:.1}%) would be exceeded ({:.1}%)", 30.0, new_cpu_percent)); }
            if !memory_ok { reasons.push(format!("Memory limit ({:.1}%) would be exceeded ({:.1}%)", 85.0, new_memory_percent)); }
            (false, Some(reasons.join(", ")))
        }
    };

    // 3. Respond
    if can_admit {
        let simulation_id = Uuid::new_v4();
        // Store the predicted cost temporarily for report_start
        state.pending_simulations.lock().await.insert(simulation_id, predicted_cost);
        tracing::info!(%simulation_id, "External run request ACCEPTED");
        Json(RequestRunResponse {
            status: "ACCEPTED".to_string(),
            simulation_id: Some(simulation_id),
            reason: None,
        })
    } else {
        tracing::warn!("External run request REJECTED: {}", reason.as_deref().unwrap_or("Unknown"));
        Json(RequestRunResponse {
            status: "REJECTED".to_string(),
            simulation_id: None,
            reason,
        })
    }
}

#[debug_handler]
async fn report_start_handler(
    State(state): State<AppState>,
    Json(payload): Json<ReportStartPayload>,
) -> impl IntoResponse {
    tracing::info!("Received report start for simulation {}: {:?}", payload.simulation_id, payload);

    let sim_id = payload.simulation_id;
    let params = SimulationParams {
        chart: payload.chart,
        node_count: payload.node_count,
        duration_secs: payload.duration_secs,
    };
    
    // Retrieve the predicted cost stored earlier
    let predicted_cost_opt = state.pending_simulations.lock().await.remove(&sim_id);
    
    let predicted_cost = match predicted_cost_opt {
        Some(cost) => {
            tracing::info!(%sim_id, "Found predicted cost for starting simulation.");
            cost
        }
        None => {
            tracing::warn!(%sim_id, "Could not find predicted cost for starting sim. Using default.");
            // Use a default/fallback if needed
            let default_cpu_per_node = if params.chart == "waku" { 0.1 } else { 0.08 };
            let default_mem_per_node = if params.chart == "waku" { 0.05 } else { 0.04 };
            ResourceCost {
                cpu_cores: params.node_count as f32 * default_cpu_per_node,
                memory_gb: params.node_count as f32 * default_mem_per_node,
                monetary_cost_eur: None, 
            }
        }
    };

    // Initialize actual_cost with the predicted cost initially
    let actual_cost = predicted_cost.clone();

    let active_sim = ActiveSimulation {
        simulation_id: sim_id,
        request_id: Uuid::new_v4(), // Placeholder: Generate new UUID. TODO: Link original request_id
        params,
        predicted_cost: predicted_cost.clone(), 
        actual_cost: actual_cost.clone(),     
        monetary_cost_eur_predicted: Some(calculate_monetary_cost(&predicted_cost, payload.duration_secs)),
        monetary_cost_eur_actual: Some(calculate_monetary_cost(&actual_cost, payload.duration_secs)),
        usage_snapshots: Vec::new(), 
        last_snapshot_time: chrono::Utc::now(),
        release_name: payload.release_name, // Real release name from the client
        issue_number: None, // Added to track origin
        status: "PendingDeployment".to_string(),
        start_time: None,
        end_time: None,
        docker_image: String::new(), // Waku specific
        pubsub_topic: String::new(), // Waku specific
        bootstrap_nodes: 0, // Waku specific (already in SimulationParams? No, add here)
        publisher_enabled: false, // Waku specific
        publisher_message_size: 0, // Waku specific
        publisher_delay: 0, // Waku specific
        publisher_message_count: 0, // Waku specific
        artificial_latency: false, // Waku specific
        latency_ms: 0, // Waku specific
        nodes_command: None, // Waku specific
        bootstrap_command: None, // Waku specific
        peer_number: None, // Nim-libp2p specific
        number_of_peers: None, // Nim-libp2p specific
        peers_to_connect: None, // Nim-libp2p specific
        message_rate: None, // Nim-libp2p specific
        message_size: None, // Nim-libp2p specific
    };

    // Add to active simulations map
    state.active_simulations.lock().await.insert(sim_id, active_sim.clone());

    // Broadcast update (calculate costs again for broadcast)
    let current_active: Vec<ActiveSimulation> = { // Scope lock
        let active_sims_map = state.active_simulations.lock().await;
        active_sims_map.values().map(|sim| {
            let mut sim_with_cost = sim.clone();
            sim_with_cost.monetary_cost_eur_predicted = Some(calculate_monetary_cost(&sim.predicted_cost, sim.params.duration_secs));
            sim_with_cost.monetary_cost_eur_actual = Some(calculate_monetary_cost(&sim.actual_cost, sim.params.duration_secs));
            sim_with_cost
        }).collect()
    }; 
    let _ = state.event_sender.send(AppEvent::ActiveUpdated(current_active));

    StatusCode::OK
}

#[debug_handler]
async fn report_complete_handler(
    State(state): State<AppState>,
    Json(payload): Json<ReportCompletePayload>,
) -> impl IntoResponse {
    tracing::info!("Received report complete for simulation {}: {:?}", payload.simulation_id, payload);
    let sim_id = payload.simulation_id;

    // Attempt to remove from pending first (if start was never reported)
    let was_pending = state.pending_simulations.lock().await.remove(&sim_id).is_some();
    if was_pending {
        tracing::warn!(%sim_id, "Simulation completed but was still pending (start never reported). Removing from pending list.");
        // Return OK because the run *did* complete, even if state was inconsistent
        return StatusCode::OK; 
    }

    // Remove from active simulations
    let completed_sim_opt = state.active_simulations.lock().await.remove(&sim_id);

    if let Some(completed_sim) = completed_sim_opt {
        // Use the actual measured cost from snapshots if available
        // otherwise fallback to provided values or default to the current actual_cost
        let final_cost = if !completed_sim.usage_snapshots.is_empty() {
            // Use the most recent snapshot for final values
            let latest_snapshot = &completed_sim.usage_snapshots[completed_sim.usage_snapshots.len() - 1];
            tracing::info!(%sim_id, 
                "Using latest measured resource values from snapshots: CPU: {:.2} cores, Memory: {:.2} GB", 
                latest_snapshot.cpu_cores, latest_snapshot.memory_gb);
            
            ResourceCost {
                cpu_cores: latest_snapshot.cpu_cores,
                memory_gb: latest_snapshot.memory_gb,
                monetary_cost_eur: None,
            }
        } else if payload.final_cpu_cores.is_some() || payload.final_memory_gb.is_some() {
            // Use provided values from payload if present
            tracing::info!(%sim_id, "Using values from payload for final cost");
            ResourceCost {
                cpu_cores: payload.final_cpu_cores.unwrap_or(completed_sim.actual_cost.cpu_cores),
                memory_gb: payload.final_memory_gb.unwrap_or(completed_sim.actual_cost.memory_gb),
                monetary_cost_eur: None,
            }
        } else {
            // Fallback to current actual_cost
            tracing::info!(%sim_id, "No snapshots or payload values available, using current actual_cost");
            completed_sim.actual_cost.clone()
        };

        // Update last finished simulation
        let last_finished = LastFinishedSimulation {
            simulation_id: sim_id,
            params: completed_sim.params.clone(),
            predicted_cost: completed_sim.predicted_cost.clone(),
            actual_cost: final_cost.clone(),
            finished_at: chrono::Utc::now(),
            duration_secs: completed_sim.params.duration_secs, // Use stored duration
        };
        *state.last_finished_simulation.lock().await = Some(last_finished.clone());

        // Broadcast updates
        let current_active: Vec<ActiveSimulation> = state.active_simulations.lock().await
            .values()
            .map(|sim| {
                let mut sim_with_cost = sim.clone();
                sim_with_cost.monetary_cost_eur_predicted = Some(calculate_monetary_cost(&sim.predicted_cost, sim.params.duration_secs));
                sim_with_cost.monetary_cost_eur_actual = Some(calculate_monetary_cost(&sim.actual_cost, sim.params.duration_secs));
                sim_with_cost
            })
            .collect();
        let _ = state.event_sender.send(AppEvent::ActiveUpdated(current_active));
        let _ = state.event_sender.send(AppEvent::LastFinished(last_finished));

        // Store final cost in DB
        let db_pool = &state.db_pool;
        let params = completed_sim.params;
        let query_result = sqlx::query(
            "INSERT OR REPLACE INTO cost_history (chart, node_count, duration_secs, cpu_cores, memory_gb, observed_at) VALUES (?, ?, ?, ?, ?, datetime('now'))"
        )
        .bind(&params.chart)
        .bind(params.node_count)
        .bind(params.duration_secs)
        .bind(final_cost.cpu_cores)
        .bind(final_cost.memory_gb)
        .execute(db_pool)
        .await;

        if let Err(e) = query_result {
            tracing::error!(%sim_id, chart = %params.chart, nodes = %params.node_count, "Failed to store final cost in DB: {}", e);
        } else {
            tracing::info!(%sim_id, 
                "Stored final cost for simulation in DB. CPU: {:.2} cores, Memory: {:.2} GB", 
                final_cost.cpu_cores, final_cost.memory_gb);
        }

        StatusCode::OK
    } else {
        tracing::warn!("Received completion report for unknown simulation ID: {} (not pending or active)", sim_id);
        StatusCode::NOT_FOUND
    }
}

// --- API Data Structures ---

// Structure for returning history via API
#[derive(Serialize, FromRow, Debug, Clone)]
pub struct SimulationRunHistoryEntry {
    simulation_id: String, // UUID as String
    request_id: String,    // UUID as String
    issue_number: Option<i64>, // Using i64 for potential large numbers, maps to INTEGER
    status: String,
    chart: String,
    node_count: i64, // Use i64 to match INTEGER
    duration_secs: i64, // Use i64 to match INTEGER
    start_time: Option<DateTime<Utc>>,
    end_time: DateTime<Utc>,
    release_name: String,
    predicted_cpu_cores: f64, // Use f64 to match REAL
    predicted_memory_gb: f64, // Use f64 to match REAL
    actual_cpu_cores: Option<f64>, // Use f64 to match REAL
    actual_memory_gb: Option<f64>, // Use f64 to match REAL
    results_url: Option<String>,
    config_details: String, // Keep as JSON string for API response
}

// --- API Handlers ---

// Fetches simulation history from the new table
#[debug_handler]
async fn api_history_handler(State(state): State<AppState>) -> impl IntoResponse {
    let db_pool = &state.db_pool;
    tracing::info!("Fetching simulation history from simulation_runs table...");

    // Query the new simulation_runs table
    let history_result: Result<Vec<SimulationRunHistoryEntry>, sqlx::Error> = sqlx::query_as(
        r#"
        SELECT 
            simulation_id, request_id, issue_number, status, chart, node_count, duration_secs,
            start_time, end_time, release_name, predicted_cpu_cores, predicted_memory_gb, 
            actual_cpu_cores, actual_memory_gb, results_url, config_details
        FROM simulation_runs 
        ORDER BY end_time DESC 
        LIMIT 100 -- Limit results for performance
        "#
    )
    .fetch_all(db_pool)
    .await;

    match history_result {
        Ok(history) => {
            tracing::info!("Successfully fetched {} history records.", history.len());
            Json(history).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to fetch simulation history: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Database error: {}", e)).into_response()
        }
    }
}

async fn sse_handler(State(state): State<AppState>) -> impl IntoResponse {
    tracing::info!("SSE client connected");
    let mut rx = state.event_sender.subscribe();

    // --- Send Initial State with Costs --- 
    let initial_queue_deque = state.queued_simulations.lock().await;
    let initial_queue_with_cost: Vec<QueuedSimulation> = initial_queue_deque.iter().map(|sim| {
        let mut sim_with_cost = sim.clone();
        sim_with_cost.predicted_cost.monetary_cost_eur = Some(calculate_monetary_cost(&sim.predicted_cost, sim.params.duration_secs));
        sim_with_cost
    }).collect();
    drop(initial_queue_deque);

    let initial_active_map = state.active_simulations.lock().await;
    let initial_active_with_cost: Vec<ActiveSimulation> = initial_active_map.values().map(|sim| {
        let mut sim_with_cost = sim.clone();
        sim_with_cost.monetary_cost_eur_predicted = Some(calculate_monetary_cost(&sim.predicted_cost, sim.params.duration_secs));
        sim_with_cost.monetary_cost_eur_actual = Some(calculate_monetary_cost(&sim.actual_cost, sim.params.duration_secs));
        sim_with_cost
    }).collect();
    drop(initial_active_map);
    
    let initial_last_finished = state.last_finished_simulation.lock().await.clone();
    let initial_cluster_util = state.cluster_utilization.lock().await.clone();
    let initial_namespace_util = state.namespace_utilization.lock().await.clone();

    let mut initial_events: Vec<Result<Event, axum::Error>> = vec![
        Ok(Event::default().json_data(AppEvent::QueueUpdated(initial_queue_with_cost)).unwrap_or_else(|e| Event::default().event("error").data(format!("Serialization error: {}", e)) )),
        Ok(Event::default().json_data(AppEvent::ActiveUpdated(initial_active_with_cost)).unwrap_or_else(|e| Event::default().event("error").data(format!("Serialization error: {}", e)) )),
        Ok(Event::default().json_data(AppEvent::ClusterUtilizationUpdated(initial_cluster_util)).unwrap_or_else(|e| Event::default().event("error").data(format!("Serialization error: {}", e)) )),
        Ok(Event::default().json_data(AppEvent::NamespaceUtilizationUpdated(initial_namespace_util)).unwrap_or_else(|e| Event::default().event("error").data(format!("Serialization error: {}", e)) )),
    ];
    
    if let Some(mut last_finished) = initial_last_finished {
        last_finished.predicted_cost.monetary_cost_eur = Some(calculate_monetary_cost(&last_finished.predicted_cost, last_finished.duration_secs));
        last_finished.actual_cost.monetary_cost_eur = Some(calculate_monetary_cost(&last_finished.actual_cost, last_finished.duration_secs));
        initial_events.push(Ok(Event::default().json_data(AppEvent::LastFinished(last_finished)).unwrap_or_else(|e| Event::default().event("error").data(format!("Serialization error: {}", e)) )));
    }

    let initial_stream = stream::iter(initial_events);

    // --- Broadcast Stream Logic --- 
    let broadcast_stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(app_event) => {
                    // Add costs before broadcasting
                    let event_with_cost = match app_event {
                        AppEvent::QueueUpdated(mut queue) => {
                            for sim in &mut queue { sim.predicted_cost.monetary_cost_eur = Some(calculate_monetary_cost(&sim.predicted_cost, sim.params.duration_secs)); }
                            AppEvent::QueueUpdated(queue)
                        },
                        AppEvent::ActiveUpdated(mut active) => {
                            for sim in &mut active {
                                sim.monetary_cost_eur_predicted = Some(calculate_monetary_cost(&sim.predicted_cost, sim.params.duration_secs));
                                sim.monetary_cost_eur_actual = Some(calculate_monetary_cost(&sim.actual_cost, sim.params.duration_secs));
                            }
                            AppEvent::ActiveUpdated(active)
                        },
                        AppEvent::LastFinished(mut finished) => {
                             finished.predicted_cost.monetary_cost_eur = Some(calculate_monetary_cost(&finished.predicted_cost, finished.duration_secs));
                             finished.actual_cost.monetary_cost_eur = Some(calculate_monetary_cost(&finished.actual_cost, finished.duration_secs));
                             AppEvent::LastFinished(finished)
                        }
                        other => other, 
                    };

                    match Event::default().json_data(&event_with_cost) {
                        Ok(event) => yield Ok(event),
                        Err(e) => {
                            tracing::error!("SSE serialization error: {}", e);
                            // Yield an error event to the client to indicate a problem
                            yield Ok(Event::default().event("error").data(format!("Server serialization error: {}", e)));
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::warn!("SSE broadcast channel closed.");
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("SSE client lagged behind by {} messages.", n);
                }
            }
        }
    };

    let stream = initial_stream.chain(broadcast_stream);

    // The stream now correctly yields Result<Event, axum::Error>
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn mock_submit_handler(State(state): State<AppState>, Json(payload): Json<MockSubmitRequest>) -> impl IntoResponse {
    tracing::info!("Received mock submit request: {:?}", payload);
    
    // Validate payload
    if payload.count == 0 {
        tracing::warn!("Received mock submit with count=0, returning 400");
        return (StatusCode::BAD_REQUEST, "count must be > 0").into_response();
    }
    
    // Try to acquire locks
    let queue_lock_result = state.queued_simulations.try_lock();
    if queue_lock_result.is_err() {
        tracing::error!("Failed to acquire queue lock: queue might be blocked");
        return (StatusCode::SERVICE_UNAVAILABLE, "Queue is currently locked by another operation").into_response();
    }
    
    let mut queue = queue_lock_result.unwrap();
    tracing::debug!("Current queue size: {}", queue.len());
    let db_pool = &state.db_pool;
    let mut new_simulations = Vec::with_capacity(payload.count as usize);

    // Create simulations
    let mut rng = StdRng::from_entropy();
    for i in 0..payload.count {
        // --- Restore parameter generation --- 
        let node_count = payload.base_nodes + rng.gen_range(0..100) * (i + 1);
        let minutes = rng.gen_range(3..=15);
        let duration_secs = minutes * 60;
        let chart_num = rng.gen_range(1..4);
        let cpu_per_node = 0.18 + (rng.gen::<f32>() * 0.04 - 0.02);
        let memory_per_node = 0.05 + (rng.gen::<f32>() * 0.02 - 0.01);
        
        tracing::debug!("Generated simulation params: chart-{}, nodes={}, duration={}s", chart_num, node_count, duration_secs);
        
        // --- Restore params initialization --- 
        let params = SimulationParams {
            chart: format!("mock-chart-{}", chart_num),
            node_count,
            duration_secs,
        };

        // --- Restore sanity check cost calculation --- 
        let sanity_check_cost = ResourceCost {
            cpu_cores: params.node_count as f32 * cpu_per_node,
            memory_gb: params.node_count as f32 * memory_per_node,
            monetary_cost_eur: None, 
        };
        
        tracing::debug!(chart = %params.chart, nodes = %params.node_count, duration = %params.duration_secs, cost = ?sanity_check_cost, "Calculated sanity check cost");
        
        // --- Restore DB lookup for predicted cost --- 
        let predicted_cost_result: Result<Option<ResourceCost>, sqlx::Error> = sqlx::query_as(
            "SELECT cpu_cores, memory_gb FROM cost_history WHERE chart = ? AND node_count = ? ORDER BY observed_at DESC LIMIT 1"
        )
        .bind(&params.chart)
        .bind(params.node_count)
        .fetch_optional(db_pool)
        .await;

        // --- Restore match block to determine predicted_cost --- 
        let predicted_cost = match predicted_cost_result {
            Ok(Some(historical_cost)) => {
                let cpu_ratio = if sanity_check_cost.cpu_cores > 0.0 { historical_cost.cpu_cores / sanity_check_cost.cpu_cores } else { 1.0 };
                let mem_ratio = if sanity_check_cost.memory_gb > 0.0 { historical_cost.memory_gb / sanity_check_cost.memory_gb } else { 1.0 };
                const MIN_RATIO: f32 = 0.5;
                const MAX_RATIO: f32 = 2.0;
                if cpu_ratio >= MIN_RATIO && cpu_ratio <= MAX_RATIO && mem_ratio >= MIN_RATIO && mem_ratio <= MAX_RATIO {
                    tracing::info!(chart = %params.chart, nodes = %params.node_count, cost = ?historical_cost, "Using historical cost from DB (within sanity range)");
                    // Ensure the monetary_cost_eur field is present, even if None initially
                    ResourceCost { monetary_cost_eur: None, ..historical_cost }
                } else {
                    tracing::warn!(chart = %params.chart, nodes = %params.node_count, historical = ?historical_cost, sanity = ?sanity_check_cost, "Historical cost out of range ({:.2}x CPU, {:.2}x Mem). Using sanity check cost.", cpu_ratio, mem_ratio);
                    sanity_check_cost 
                }
            }
            Ok(None) => {
                tracing::info!(chart = %params.chart, nodes = %params.node_count, cost = ?sanity_check_cost, "No history found, using sanity check cost");
                sanity_check_cost 
            }
            Err(e) => {
                tracing::error!(chart = %params.chart, nodes = %params.node_count, "DB error fetching cost: {}. Using sanity check cost.", e);
                sanity_check_cost 
            }
        };

        // 1. Define the queued_sim first
        let queued_sim = QueuedSimulation {
            request_id: Uuid::new_v4(),
            params: params.clone(), // Use the generated params
            predicted_cost, // Use the determined predicted cost
            issue_number: None, // Added to track origin
            docker_image: String::new(), // Waku specific
            pubsub_topic: String::new(), // Waku specific
            bootstrap_nodes: None, // Waku specific (ADDED)
            publisher_enabled: false, // Waku specific
            publisher_message_size: 0, // Waku specific
            publisher_delay: 0, // Waku specific
            publisher_message_count: 0, // Waku specific
            artificial_latency: false, // Waku specific
            latency_ms: 0, // Waku specific
            nodes_command: None, // Waku specific
            bootstrap_command: None, // Waku specific
            peer_number: None, // Nim-libp2p specific
            number_of_peers: None, // Nim-libp2p specific
            peers_to_connect: None, // Nim-libp2p specific
            message_rate: None, // Nim-libp2p specific
            message_size: None, // Nim-libp2p specific
            parallel_limit: 1, // Common
        };

        tracing::debug!(request_id = %queued_sim.request_id, "Created queued simulation");

        // 2. Clone it and calculate monetary cost
        let mut sim_with_cost = queued_sim.clone(); 
        sim_with_cost.predicted_cost.monetary_cost_eur = Some(calculate_monetary_cost(
            &sim_with_cost.predicted_cost, 
            sim_with_cost.params.duration_secs 
        ));

        // 3. Add the version with cost to the collection
        new_simulations.push(sim_with_cost);
    }
    
    // Add all simulations to the queue
    tracing::debug!("Adding {} new simulations to queue", new_simulations.len());
    for sim in new_simulations.iter() {
        queue.push_back(sim.clone());
    }
    let queue_len = queue.len();
    tracing::debug!("Queue size after adding: {}", queue_len);

    // Broadcast the queue update with calculated costs
    let queue_updated: Vec<QueuedSimulation> = queue.iter().map(|sim| { 
        // Ensure cost is calculated (it should be from when added, but recalculate for safety)
        let mut sim_with_cost = sim.clone();
        sim_with_cost.predicted_cost.monetary_cost_eur = Some(calculate_monetary_cost(
            &sim.predicted_cost, 
            sim.params.duration_secs
        ));
        sim_with_cost // Explicitly return the modified simulation
    }).collect(); // Now collecting QueuedSimulation items
    drop(queue);
    
    tracing::debug!("Broadcasting queue update with {} items", queue_updated.len());
    let result = state.event_sender.send(AppEvent::QueueUpdated(queue_updated));
    match result {
        Ok(receivers) => tracing::debug!("Queue update sent to {} receivers", receivers),
        Err(e) => tracing::error!("Failed to broadcast queue update: {:?}", e),
    }
    
    tracing::info!("Added {} mock simulations to queue (size: {})", payload.count, queue_len);
    StatusCode::OK.into_response()
}

async fn set_time_dilation_handler(State(state): State<AppState>, Json(payload): Json<TimeDilationRequest>) -> impl IntoResponse {
    // Actual implementation...
    let mut time_dilation = state.time_dilation.lock().await;
    let factor = match payload.factor { 1 | 3 | 5 | 10 => payload.factor, _ => 1 };
    *time_dilation = factor;
    tracing::info!("Time dilation set to {}x", factor);
    StatusCode::OK
}

// ... rest of main.rs ...

// --- Mocking Routes --- 
// .route("/mock_submit", post(mock_submit_handler))
// .route("/set_time_dilation", post(set_time_dilation_handler))
// .route("/api/v1/request_run", post(request_run_handler))
// .route("/api/v1/report_start", post(report_start_handler))
// .route("/api/v1/report_complete", post(report_complete_handler))

/// Process the next simulation from the queue, moving it to active status
/// Returns Ok(true) if a simulation was processed, Ok(false) if no simulations were in the queue
async fn process_next_simulation_from_queue(state: &AppState) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let mut queued_simulations = state.queued_simulations.lock().await;
    
    if let Some(queued_sim) = queued_simulations.front() {
        tracing::info!(
            issue = queued_sim.issue_number.map(|i| i.to_string()).unwrap_or_else(|| "N/A".to_string()),
            chart = %queued_sim.params.chart,
            nodes = queued_sim.params.node_count,
            "Checking admission for next queued simulation"
        );

        // --- Check Admission Control (using predicted_cost from the queued sim) ---
        let can_admit = {
            let cluster_util = state.cluster_utilization.lock().await;
            let predicted_cpu = queued_sim.predicted_cost.cpu_cores;
            let predicted_memory = queued_sim.predicted_cost.memory_gb;
            
            // TODO: Consider active simulations already running but maybe not fully measured yet?
            // For now, use the current measured utilization + prediction.
            let new_total_cpu = cluster_util.used_cpu_cores + predicted_cpu;
            let new_total_memory = cluster_util.used_memory_gb + predicted_memory;
            
            // Use configured limits (e.g., from env vars or defaults)
            // Hardcoding for now, should make configurable
            let cpu_limit_percent = 80.0;
            let memory_limit_percent = 85.0;

            let new_cpu_percent = (new_total_cpu / cluster_util.total_cpu_cores) * 100.0;
            let new_memory_percent = (new_total_memory / cluster_util.total_memory_gb) * 100.0;
            
            let cpu_ok = new_cpu_percent <= cpu_limit_percent;
            let memory_ok = new_memory_percent <= memory_limit_percent;
            
            if cpu_ok && memory_ok {
                true
            } else {
                let mut reasons = Vec::new();
                if !cpu_ok { reasons.push(format!("CPU limit ({:.1}%) would be exceeded ({:.1}%)", cpu_limit_percent, new_cpu_percent)); }
                if !memory_ok { reasons.push(format!("Memory limit ({:.1}%) would be exceeded ({:.1}%)", memory_limit_percent, new_memory_percent)); }
                tracing::warn!(
                    issue = queued_sim.issue_number.map(|i| i.to_string()).unwrap_or_else(|| "N/A".to_string()),
                    reason = reasons.join(", "), 
                    "Admission rejected for queued simulation"
                );
                false
            }
        };

        if can_admit {
            // Remove from queue and prepare to run
            let sim_to_run = queued_simulations.pop_front().unwrap(); // Safe due to check above
            drop(queued_simulations); // Release lock on queue

            let simulation_id = Uuid::new_v4();
            let chart = &sim_to_run.params.chart;
            let node_count = sim_to_run.params.node_count;
            let index_placeholder = sim_to_run.request_id.to_string().chars().take(6).collect::<String>(); // Use part of request ID for uniqueness

            // Generate Helm Release Name (similar to Python script)
            let k_value = node_count as f32 / 1000.0;
            let k_str = if k_value >= 1.0 {
                format!("{}k", k_value as u32)
            } else {
                node_count.to_string()
            };
            
            let release_name = if chart == "waku" {
                 format!(
                    "waku-{}-{}mgs-{}s-{}kb-{}", 
                    k_str, 
                    sim_to_run.publisher_message_size, // Note: Python used delay here? Let's use size.
                    sim_to_run.publisher_delay, 
                    sim_to_run.publisher_message_size, // Size again? Let's use message size
                    index_placeholder 
                 )
            } else { // nimlibp2p
                 format!(
                    "nimlibp2p-{}-{}mgs-{}kb-{}", 
                    k_str, // Based on number_of_peers
                    sim_to_run.message_rate.unwrap_or(0),
                    sim_to_run.message_size.unwrap_or(0),
                    index_placeholder
                 )
            };

            // Create ActiveSimulation entry
            let active_sim = ActiveSimulation {
                simulation_id, 
                request_id: sim_to_run.request_id,
                params: sim_to_run.params.clone(),
                predicted_cost: sim_to_run.predicted_cost.clone(),
                actual_cost: ResourceCost::default(), // Initialize actual cost
                usage_snapshots: Vec::new(),
                last_snapshot_time: chrono::Utc::now(),
                monetary_cost_eur_predicted: Some(calculate_monetary_cost(&sim_to_run.predicted_cost, sim_to_run.params.duration_secs)),
                monetary_cost_eur_actual: None, // Set later
                release_name: release_name.clone(), 
                issue_number: sim_to_run.issue_number,
                status: "PendingDeployment".to_string(),
                start_time: None,
                end_time: None,
                docker_image: sim_to_run.docker_image,
                pubsub_topic: sim_to_run.pubsub_topic,
                bootstrap_nodes: sim_to_run.bootstrap_nodes.unwrap_or(3), // Default if None (shouldn't happen for waku)
                publisher_enabled: sim_to_run.publisher_enabled,
                publisher_message_size: sim_to_run.publisher_message_size,
                publisher_delay: sim_to_run.publisher_delay,
                publisher_message_count: sim_to_run.publisher_message_count,
                artificial_latency: sim_to_run.artificial_latency,
                latency_ms: sim_to_run.latency_ms,
                nodes_command: sim_to_run.nodes_command,
                bootstrap_command: sim_to_run.bootstrap_command,
                peer_number: sim_to_run.peer_number,
                number_of_peers: sim_to_run.number_of_peers,
                peers_to_connect: sim_to_run.peers_to_connect,
                message_rate: sim_to_run.message_rate,
                message_size: sim_to_run.message_size,
            };

            // Add to active simulations map
            state.active_simulations.lock().await.insert(simulation_id, active_sim.clone());

            // Broadcast updated active list
            let current_active = state.active_simulations.lock().await.values().cloned().collect();
            let _ = state.event_sender.send(AppEvent::ActiveUpdated(current_active));
            
            // Broadcast updated queue list
            let current_queue = state.queued_simulations.lock().await.iter().cloned().collect();
            let _ = state.event_sender.send(AppEvent::QueueUpdated(current_queue));

            tracing::info!(%simulation_id, %release_name, issue = sim_to_run.issue_number.map(|i| i.to_string()).unwrap_or_else(|| "N/A".to_string()), "Admitted and spawning simulation task.");

            // Spawn the actual simulation task
            let task_state = state.clone();
            tokio::spawn(run_simulation_task(task_state, active_sim));

            return Ok(true); // Indicate a simulation was started

        } else {
            // Cannot admit, leave it in the queue for next time
            return Ok(false); // Indicate no simulation started (due to admission control)
        }

    } else {
        // Queue is empty
        tracing::debug!("Simulation queue is empty.");
        return Ok(false); // Indicate no simulation started (queue empty)
    }
}

// --- Simulation Execution Task ---

// This task runs a single simulation (Helm, wait, cleanup)
async fn run_simulation_task(state: AppState, mut sim: ActiveSimulation) {
    let sim_id = sim.simulation_id;
    let release_name = sim.release_name.clone();
    let namespace = "larstesting"; // Make configurable?
    let chart_type = sim.params.chart.clone();
    let duration_seconds = sim.params.duration_secs;
    let issue_str = sim.issue_number.map(|i| format!("#{}", i)).unwrap_or_else(|| "N/A".to_string());

    // Update status
    update_simulation_status(&state, sim_id, "Deploying").await;
    sim.start_time = Some(chrono::Utc::now()); // Record deployment start

    // --- 1. Generate values.yaml --- 
    let values_yaml = match generate_values_yaml(&sim) {
        Ok(yaml) => yaml,
        Err(e) => {
            tracing::error!(%sim_id, issue = %issue_str, "Failed to generate values.yaml: {}", e);
            update_simulation_status(&state, sim_id, "Failed (Config)").await;
            cleanup_failed_simulation(&state, sim.clone(), &release_name, namespace).await;
            return;
        }
    };
    
    // --- 2. Create Temp File for values.yaml ---
    let values_file_path_result = write_temp_values_file(&values_yaml).await; // Added .await
    if let Err(e) = values_file_path_result {
        tracing::error!(%sim_id, issue = %issue_str, "Failed to write temp values file: {}", e);
        update_simulation_status(&state, sim_id, "Failed (IO)").await;
        cleanup_failed_simulation(&state, sim.clone(), &release_name, namespace).await; // Pass sim object
        return;
    }
    let values_file_path = values_file_path_result.unwrap(); // Safe due to check above

    // --- 3. Run Helm Upgrade/Install --- 
    let helm_result = run_helm_deploy(&sim, &release_name, namespace, &values_file_path).await; // Added .await
    
    // Clean up temp file regardless of helm outcome
    if let Err(e) = tokio::fs::remove_file(&values_file_path).await {
        tracing::warn!(%sim_id, path = %values_file_path.display(), "Failed to remove temp values file: {}", e);
    }

    if let Err(e) = helm_result {
        tracing::error!(%sim_id, issue = %issue_str, release = %release_name, "Helm deployment failed: {}", e);
        update_simulation_status(&state, sim_id, "Failed (Helm)").await;
        // Attempt cleanup even if deploy failed
        cleanup_failed_simulation(&state, sim.clone(), &release_name, namespace).await; // Pass sim object
        return;
    }
    tracing::info!(%sim_id, issue = %issue_str, release = %release_name, "Helm deployment initiated successfully.");

    // --- 4. Wait for StatefulSet Rollout --- 
    update_simulation_status(&state, sim_id, "WaitingForRollout").await;
    // Construct the StatefulSet name (assuming chart follows pattern)
    let statefulset_name = format!("{}-nodes", release_name); // Adapt if nim-libp2p uses different naming
    let rollout_result = run_kubectl_rollout(&statefulset_name, namespace).await;

    if let Err(e) = rollout_result {
        tracing::error!(%sim_id, issue = %issue_str, release = %release_name, sts = %statefulset_name, "Rollout status check failed: {}", e);
        update_simulation_status(&state, sim_id, "Failed (Rollout)").await;
        // Attempt cleanup
        cleanup_simulation(&state, sim.clone(), &release_name, namespace, "Failed (Rollout)").await; // Pass sim object
        return;
    }
    tracing::info!(%sim_id, issue = %issue_str, release = %release_name, sts = %statefulset_name, "StatefulSet rollout successful.");
    sim.start_time = Some(chrono::Utc::now()); // Record actual simulation start time after rollout

    // --- 5. Wait for Simulation Duration --- 
    update_simulation_status(&state, sim_id, "Running").await;
    tracing::info!(%sim_id, issue = %issue_str, release = %release_name, duration = duration_seconds, "Waiting for simulation duration...");
    tokio::time::sleep(Duration::from_secs(duration_seconds as u64)).await;
    sim.end_time = Some(chrono::Utc::now());
    tracing::info!(%sim_id, issue = %issue_str, release = %release_name, "Simulation duration finished.");

    // --- 6. Cleanup Simulation --- 
    // Pass the simulation object itself so cleanup can potentially use its final state
    let final_sim_state = cleanup_simulation(&state, sim, &release_name, namespace, "Completed").await;

    // --- 7. Trigger Analysis and Posting --- 
    if let Some(completed_sim) = final_sim_state {
        tracing::info!(sim_id=%completed_sim.simulation_id, issue = %issue_str, "Simulation task finished. Triggering analysis and result posting.");
        // Spawn analysis as a separate task to avoid blocking the scheduler if analysis is slow
        let analysis_state = state.clone();
        tokio::spawn(run_analysis_and_post_results(analysis_state, completed_sim));
    } else {
        tracing::warn!(sim_id=%sim_id, issue = %issue_str, "Simulation cleanup didn't return final state. Skipping analysis.");
    }
}

// --- Analysis and GitHub Posting ---

#[derive(Serialize)]
struct ScrapeGeneralConfig {
    times_names: Vec<Vec<String>>,
}

#[derive(Serialize)]
struct ScrapeConfigSection {
    #[serde(rename = "$__rate_interval")]
    rate_interval: String,
    step: String,
    dump_location: String,
}

#[derive(Serialize, Clone)]
struct ScrapeMetric {
    query: String,
    extract_field: String,
    folder_name: String,
}

#[derive(Serialize, Clone)]
struct ScrapePlottingConfig {
    ignore_columns: Vec<String>,
    data_points: u32,
    folder: Vec<String>,
    data: Vec<String>,
    include_files: Vec<String>,
    xlabel_name: String,
    ylabel_name: String,
    show_min_max: bool,
    outliers: bool,
    #[serde(rename = "scale-x")]
    scale_x: u32,
    fig_size: [u32; 2],
}

#[derive(Serialize)]
struct ScrapeYamlRoot {
    general_config: ScrapeGeneralConfig,
    scrape_config: ScrapeConfigSection,
    metrics_to_scrape: std::collections::HashMap<String, ScrapeMetric>,
    plotting: std::collections::HashMap<String, ScrapePlottingConfig>,
}

// Generates the scrape.yaml content for a completed simulation
fn generate_scrape_yaml_rust(sim: &ActiveSimulation) -> Result<String, serde_yaml::Error> {
    let start_time_str = sim.start_time.map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default();
    let end_time_str = sim.end_time.map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default();
    let release_name = sim.release_name.clone();

    // The python script expects a list of lists for times_names
    let times_names_data = vec![vec![start_time_str, end_time_str, release_name.clone()]];
    
    // Define a unique dump location for this simulation's metrics
    let dump_location = format!("lars_metrics/{}/", sim.simulation_id); // Removed comma
    let plot_name = format!("plot-{}", sim.simulation_id);

    let scrape_config = ScrapeYamlRoot {
        general_config: ScrapeGeneralConfig {
            times_names: times_names_data,
        },
        scrape_config: ScrapeConfigSection {
            rate_interval: "121s".to_string(),
            step: "60s".to_string(),
            dump_location: dump_location.clone(), // Use the unique dump location
        },
        metrics_to_scrape: [
            ("libp2p_network_in".to_string(), ScrapeMetric {
                query: "rate(libp2p_network_bytes_total{direction='in'}[$__rate_interval])".to_string(),
                extract_field: "instance".to_string(),
                folder_name: "libp2p-in/".to_string(),
            }),
            ("libp2p_network_out".to_string(), ScrapeMetric {
                query: "rate(libp2p_network_bytes_total{direction='out'}[$__rate_interval])".to_string(),
                extract_field: "instance".to_string(),
                folder_name: "libp2p-out/".to_string(),
            })
        ].iter().cloned().collect(),
        plotting: [
            (plot_name, ScrapePlottingConfig {
                ignore_columns: vec!["bootstrap".to_string(), "midstrap".to_string()],
                data_points: 25,
                folder: vec![dump_location], // Plot data from this sim's dump location
                data: vec!["libp2p-in".to_string(), "libp2p-out".to_string()],
                include_files: vec![release_name], // Only include this simulation's release name
                xlabel_name: "Simulation".to_string(),
                ylabel_name: "KBytes/s".to_string(),
                show_min_max: false,
                outliers: true,
                scale_x: 1000,
                fig_size: [20, 20],
            })
        ].iter().cloned().collect(),
    };

    // Serialize to YAML string
    // Note: serde_yaml doesn't perfectly replicate the python script's times_names format easily.
    // We might need to adjust the python script slightly to parse this format,
    // or do more complex serialization here.
    // For now, this standard YAML should be parseable.
    serde_yaml::to_string(&scrape_config)
}

// Main function to orchestrate analysis and posting results
async fn run_analysis_and_post_results(state: AppState, sim: ActiveSimulation) {
    let sim_id = sim.simulation_id;
    let issue_number = match sim.issue_number {
        Some(num) => num,
        None => {
            tracing::warn!(%sim_id, "Cannot post results as simulation is not linked to a GitHub issue.");
            return;
        }
    };
    let issue_str = format!("#{}", issue_number);
    tracing::info!(%sim_id, issue = %issue_str, "Starting analysis process...");

    // --- 1. Generate scrape.yaml ---
    let scrape_yaml_content = match generate_scrape_yaml_rust(&sim) {
        Ok(content) => content,
        Err(e) => {
            tracing::error!(%sim_id, issue = %issue_str, "Failed to generate scrape.yaml content: {}", e);
            // TODO: Maybe post a failure comment to GitHub?
            return;
        }
    };

    // --- 2. Write scrape.yaml to temp file ---
    let scrape_yaml_path = match write_temp_scrape_file(&scrape_yaml_content, sim_id).await {
        Ok(path) => path,
        Err(e) => {
             tracing::error!(%sim_id, issue = %issue_str, "Failed to write temporary scrape.yaml: {}", e);
             // TODO: Maybe post a failure comment to GitHub?
             return;
        }
    };

    // --- 3. Run Python Analysis Script --- 
    // Define path for the output PNG
    let analysis_dir = format!("./analysis_results/{}", sim_id);
    if let Err(e) = tokio::fs::create_dir_all(&analysis_dir).await {
         tracing::error!(%sim_id, issue = %issue_str, path=%analysis_dir, "Failed to create analysis output directory: {}", e);
         cleanup_temp_file(&scrape_yaml_path).await;
         return;
    }
    let output_png_path = format!("{}/analysis_plot.png", analysis_dir);

    let analysis_result = run_python_analysis_script(&scrape_yaml_path, &output_png_path).await;
    
    // Clean up scrape.yaml file now that analysis is done (or failed)
    cleanup_temp_file(&scrape_yaml_path).await;

    let png_file_path = match analysis_result {
        Ok(path) => path,
        Err(e) => {
            tracing::error!(%sim_id, issue = %issue_str, "Python analysis script failed: {}", e);
            // TODO: Post failure comment to GitHub?
            return;
        }
    };
    tracing::info!(%sim_id, issue = %issue_str, png_path=%png_file_path, "Python analysis script completed successfully.");

    // --- 4. Commit PNG to GitHub Repo --- 
    let commit_result = commit_analysis_to_github(&state, issue_number, &png_file_path).await;
    
    // --- Store results_url in DB --- 
    let results_url_for_db: Option<String> = match &commit_result {
        Ok(url) => Some(url.clone()),
        Err(_) => None,
    };
    if let Some(url) = &results_url_for_db {
         let update_url_result = sqlx::query("UPDATE simulation_runs SET results_url = ? WHERE simulation_id = ?")
            .bind(url)
            .bind(sim.simulation_id.to_string())
            .execute(&state.db_pool)
            .await;
        if let Err(e) = update_url_result {
             tracing::error!(sim_id=%sim.simulation_id, "Failed to update results_url in simulation_runs: {}", e);
        }
    }

    let github_file_url = match commit_result {
        Ok(url) => url,
        Err(e) => {
             tracing::error!(%sim_id, issue = %issue_str, "Failed to commit analysis PNG to GitHub: {}", e);
             // TODO: Post failure comment (without image)?
             // Try to update labels anyway
             update_github_labels_post_analysis(&state, issue_number, sim_id, false).await;
             return;
        }
    };
    tracing::info!(%sim_id, issue = %issue_str, url=%github_file_url, "Successfully committed analysis PNG to GitHub.");

    // --- 5. Post Comment to GitHub Issue --- 
    let comment_body = format!("# Results\n\n![Analysis Plot]({})", github_file_url);
    if let Err(e) = post_comment_to_github(&state, issue_number, &comment_body).await {
         tracing::error!(%sim_id, issue = %issue_str, "Failed to post results comment to GitHub: {}", e);
         // Continue to labeling even if comment fails
    }
    tracing::info!(%sim_id, issue = %issue_str, "Successfully posted results comment to GitHub.");

    // --- 6. Update GitHub Labels --- 
    update_github_labels_post_analysis(&state, issue_number, sim_id, true).await;

    tracing::info!(%sim_id, issue = %issue_str, "Analysis and result posting complete.");
}

// Helper to write scrape.yaml to a temporary file
async fn write_temp_scrape_file(content: &str, sim_id: Uuid) -> Result<std::path::PathBuf, std::io::Error> {
    use tokio::io::AsyncWriteExt;
    let temp_dir = std::env::temp_dir().join("lars_scrape");
    tokio::fs::create_dir_all(&temp_dir).await?;
    let file_path = temp_dir.join(format!("scrape-{}.yaml", sim_id));
    let mut file = tokio::fs::File::create(&file_path).await?;
    file.write_all(content.as_bytes()).await?;
    Ok(file_path)
}

// Helper to clean up a temporary file
async fn cleanup_temp_file(path: &std::path::Path) {
    if let Err(e) = tokio::fs::remove_file(path).await {
        tracing::warn!(path = %path.display(), "Failed to remove temporary file: {}", e);
    }
}

// Placeholder function to run the python analysis script
async fn run_python_analysis_script(scrape_yaml_path: &std::path::Path, output_png_path: &str) -> Result<String, String> {
    let script_path = "./10ksim/analyse.py"; // Relative to LARS execution dir
    let python_executable = "python3"; // Assume python3 is in PATH

    // --- Ensure 10ksim repo exists ---
    if !tokio::fs::try_exists("./10ksim").await.unwrap_or(false) {
        tracing::info!("Cloning 10ksim repository...");
        let mut clone_cmd = tokio::process::Command::new("git");
        clone_cmd.arg("clone").arg("https://github.com/vacp2p/10ksim.git").arg("./10ksim");
        let clone_output = clone_cmd.output().await.map_err(|e| format!("Failed to execute git clone: {}", e))?;
        if !clone_output.status.success() {
            return Err(format!("git clone failed: {}\n{}", 
                String::from_utf8_lossy(&clone_output.stderr),
                String::from_utf8_lossy(&clone_output.stdout)
            ));
        }
    }
    
    // --- Create a symlink or copy scrape.yaml to the expected location ---
    // Since the Python script expects scrape.yaml in the current directory,
    // we need to make sure our generated file is available at that location
    tracing::info!("Copying scrape YAML to current directory");
    tokio::fs::copy(scrape_yaml_path, "scrape.yaml").await
        .map_err(|e| format!("Failed to copy scrape.yaml to current directory: {}", e))?;
    
    // --- Execute Script (without arguments, like the Python implementation) --- 
    tracing::info!("Running Python analysis script: {}", script_path);
    let mut cmd = tokio::process::Command::new(python_executable);
    cmd.arg(script_path);
    // No additional arguments - matching the Python run.py implementation

    let output = cmd.output().await.map_err(|e| format!("Failed to execute python script: {}", e))?;

    if !output.status.success() {
        Err(format!("Python script failed.\nExit Code: {}\nStderr: {}\nStdout: {}", 
            output.status, 
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        ))
    } else {
        // Check if the output file was actually created
        if tokio::fs::try_exists(output_png_path).await.unwrap_or(false) {
             Ok(output_png_path.to_string())
        } else {
             Err(format!("Python script succeeded but output PNG file '{}' was not found. Script stdout:\n{}", 
                output_png_path,
                String::from_utf8_lossy(&output.stdout)
            ))
        }
    }
}

// Placeholder function to commit the analysis PNG to GitHub
async fn commit_analysis_to_github(state: &AppState, issue_number: u64, local_png_path: &str) -> Result<String, String> {
    // Strategy: Commit to a known path in the repo, e.g., simulation_results/{issue_number}/analysis_plot.png
    let repo_path = format!("simulation_results/{}/analysis_plot.png", issue_number);
    tracing::info!(path=%repo_path, "Attempting to commit analysis PNG to GitHub repo");

    // 1. Read file content
    let content_bytes = tokio::fs::read(local_png_path).await
        .map_err(|e| format!("Failed to read local PNG file '{}': {}", local_png_path, e))?;
    
    // 2. Base64 encode content
    let content_base64 = base64::engine::general_purpose::STANDARD.encode(&content_bytes); // Use Engine trait

    // 3. Check if file already exists (to get SHA for update)
    let get_url = format!("https://api.github.com/repos/{}/contents/{}", state.github_repo, repo_path);
    let headers = build_github_headers(&state.github_token)?;
    
    let get_response = state.http_client.get(&get_url).headers(headers.clone()).send().await
         .map_err(|e| format!("GitHub API request failed (GET {}): {}", repo_path, e))?;

    let existing_sha = if get_response.status() == reqwest::StatusCode::OK {
        #[derive(Deserialize)]
        struct GetContentResponse { sha: String }
        let json_body: GetContentResponse = get_response.json().await
             .map_err(|e| format!("Failed to parse GitHub GET response for {}: {}", repo_path, e))?;
        Some(json_body.sha)
    } else if get_response.status() == reqwest::StatusCode::NOT_FOUND {
        None
    } else {
        return Err(format!("GitHub API error checking file existence (GET {}): Status {}, Body: {}", 
            repo_path, get_response.status(), get_response.text().await.unwrap_or_default()));
    };

    // 4. Create or Update file content via API
    let put_url = get_url; // Same URL for PUT
    let commit_message = format!("Add analysis results for simulation from issue #{}", issue_number);

    #[derive(Serialize)]
    struct PutContentPayload {
        message: String,
        content: String, // base64 encoded
        #[serde(skip_serializing_if = "Option::is_none")]
        sha: Option<String>, // Required if updating
    }

    let payload = PutContentPayload {
        message: commit_message,
        content: content_base64,
        sha: existing_sha,
    };

    let put_response = state.http_client.put(&put_url)
        .headers(headers.clone()) // Reuse headers
        .json(&payload)
        .send().await
        .map_err(|e| format!("GitHub API request failed (PUT {}): {}", repo_path, e))?;

    if put_response.status() == reqwest::StatusCode::OK || put_response.status() == reqwest::StatusCode::CREATED {
        #[derive(Deserialize)]
        struct PutContentResponse { content: ContentDetails }
        #[derive(Deserialize)]
        struct ContentDetails { html_url: String }
        
        let json_body: PutContentResponse = put_response.json().await
             .map_err(|e| format!("Failed to parse GitHub PUT response for {}: {}", repo_path, e))?;
        Ok(json_body.content.html_url)
    } else {
        Err(format!("GitHub API error committing file (PUT {}): Status {}, Body: {}", 
            repo_path, put_response.status(), put_response.text().await.unwrap_or_default()))
    }
}

// Placeholder function to post a comment to a GitHub issue
async fn post_comment_to_github(state: &AppState, issue_number: u64, body: &str) -> Result<(), String> {
    let url = format!("https://api.github.com/repos/{}/issues/{}/comments", state.github_repo, issue_number);
    let headers = build_github_headers(&state.github_token)?;

    #[derive(Serialize)]
    struct CommentPayload { body: String }
    let payload = CommentPayload { body: body.to_string() };

    let response = state.http_client.post(&url)
        .headers(headers)
        .json(&payload)
        .send().await
        .map_err(|e| format!("GitHub API request failed (POST comment issue {}): {}", issue_number, e))?;

    if response.status() == reqwest::StatusCode::CREATED {
        Ok(())
    } else {
        Err(format!("GitHub API error posting comment (issue {}): Status {}, Body: {}", 
            issue_number, response.status(), response.text().await.unwrap_or_default()))
    }
}

// Placeholder function to add a label to a GitHub issue
async fn add_github_label(state: &AppState, issue_number: u64, label: &str) -> Result<(), String> {
    let url = format!("https://api.github.com/repos/{}/issues/{}/labels", state.github_repo, issue_number);
    let headers = build_github_headers(&state.github_token)?;

    #[derive(Serialize)]
    struct LabelsPayload { labels: Vec<String> }
    let payload = LabelsPayload { labels: vec![label.to_string()] };

     let response = state.http_client.post(&url)
        .headers(headers)
        .json(&payload)
        .send().await
        .map_err(|e| format!("GitHub API request failed (POST label issue {}): {}", issue_number, e))?;

    // GitHub returns 200 OK on success for adding labels
    if response.status() == reqwest::StatusCode::OK {
        Ok(())
    } else {
         Err(format!("GitHub API error adding label '{}' (issue {}): Status {}, Body: {}", 
            label, issue_number, response.status(), response.text().await.unwrap_or_default()))
    }
}

// Placeholder function to remove a label from a GitHub issue
async fn remove_github_label(state: &AppState, issue_number: u64, label: &str) -> Result<(), String> {
    // URL encode the label name in case it contains special characters
    let encoded_label = urlencoding::encode(label);
    let url = format!("https://api.github.com/repos/{}/issues/{}/labels/{}", state.github_repo, issue_number, encoded_label);
    let headers = build_github_headers(&state.github_token)?;

    let response = state.http_client.delete(&url)
        .headers(headers)
        .send().await
        .map_err(|e| format!("GitHub API request failed (DELETE label issue {}): {}", issue_number, e))?;

    // GitHub returns 200 OK or 204 No Content on success, 404 if label not found (which is fine)
    if response.status().is_success() || response.status() == reqwest::StatusCode::NOT_FOUND {
        Ok(())
    } else {
         Err(format!("GitHub API error removing label '{}' (issue {}): Status {}, Body: {}", 
            label, issue_number, response.status(), response.text().await.unwrap_or_default()))
    }
}

// Helper to build common GitHub API headers
fn build_github_headers(token: &str) -> Result<reqwest::header::HeaderMap, String> {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "Authorization", 
        format!("token {}", token).parse().map_err(|e| format!("Invalid GitHub token format: {}", e))?
    );
    headers.insert(
        "Accept", 
        "application/vnd.github.v3+json".parse().map_err(|_| "Invalid Accept header".to_string())?
    );
    headers.insert(
        "User-Agent", 
        "LARS-Simulation-Scheduler".parse().map_err(|_| "Invalid User-Agent header".to_string())?
    );
    Ok(headers)
}


// --- Helper to handle cleanup after a simulation ---

// Updated to return the final state of the simulation upon successful removal
async fn cleanup_simulation(state: &AppState, sim_to_clean: ActiveSimulation, release_name: &str, namespace: &str, final_status: &str) -> Option<ActiveSimulation> {
    let sim_id = sim_to_clean.simulation_id;
    update_simulation_status(state, sim_id, "CleaningUp").await;
    if let Err(e) = run_helm_uninstall(release_name, namespace).await {
        tracing::error!(%sim_id, release=%release_name, "Helm cleanup failed during final cleanup: {}", e);
    }

    // Remove from active map first
    let mut active_sims = state.active_simulations.lock().await;
    let finished_sim_opt = active_sims.remove(&sim_id);
    
    // Broadcast updated active list regardless of whether removal succeeded
    let current_active = active_sims.values().cloned().collect();
    drop(active_sims); // Release lock before broadcasting and DB operations
    let _ = state.event_sender.send(AppEvent::ActiveUpdated(current_active));

    if let Some(mut finished_sim) = finished_sim_opt {
        finished_sim.status = final_status.to_string();
        let end_time = finished_sim.end_time.unwrap_or_else(|| chrono::Utc::now()); // Ensure end time is set
        finished_sim.end_time = Some(end_time);

        // Determine final actual cost
        let final_actual_cost = finished_sim.usage_snapshots.last().map(|snap| 
            ResourceCost { cpu_cores: snap.cpu_cores, memory_gb: snap.memory_gb, monetary_cost_eur: None }
        );

        // Update last_finished_simulation state (for UI)
        let last_finished = LastFinishedSimulation {
            simulation_id: finished_sim.simulation_id,
            params: finished_sim.params.clone(),
            predicted_cost: finished_sim.predicted_cost.clone(),
            // Use final actual cost if available, otherwise predicted cost
            actual_cost: final_actual_cost.clone().unwrap_or_else(|| finished_sim.predicted_cost.clone()),
            finished_at: end_time,
            duration_secs: finished_sim.params.duration_secs,
        };
        *state.last_finished_simulation.lock().await = Some(last_finished.clone());
        let _ = state.event_sender.send(AppEvent::LastFinished(last_finished));
        
        // --- Database Updates ---
        let db_pool = &state.db_pool;

        // 1. Serialize config details to JSON
        let config_details_json = match serde_json::to_string(&finished_sim) {
            Ok(json) => json,
            Err(e) => {
                tracing::error!(%sim_id, "Failed to serialize simulation config to JSON: {}", e);
                "{}".to_string() // Store empty JSON object on error
            }
        };

        // 2. Insert into simulation_runs table
        let insert_run_result = sqlx::query(
            r#"
            INSERT INTO simulation_runs (
                simulation_id, request_id, issue_number, status, chart, node_count, duration_secs,
                start_time, end_time, release_name, predicted_cpu_cores, predicted_memory_gb, 
                actual_cpu_cores, actual_memory_gb, results_url, config_details
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#
        )
        .bind(finished_sim.simulation_id.to_string()) // Store UUID as string
        .bind(finished_sim.request_id.to_string())
        .bind(finished_sim.issue_number.map(|i| i as i64)) // Use i64
        .bind(&finished_sim.status)
        .bind(&finished_sim.params.chart)
        .bind(finished_sim.params.node_count as i64)
        .bind(finished_sim.params.duration_secs as i64)
        .bind(finished_sim.start_time)
        .bind(end_time)
        .bind(&finished_sim.release_name)
        .bind(finished_sim.predicted_cost.cpu_cores as f64)
        .bind(finished_sim.predicted_cost.memory_gb as f64)
        .bind(final_actual_cost.as_ref().map(|c| c.cpu_cores as f64)) // Option<f64>
        .bind(final_actual_cost.as_ref().map(|c| c.memory_gb as f64)) // Option<f64>
        .bind::<Option<String>>(None) // Placeholder for results_url, update later
        .bind(config_details_json)
        .execute(db_pool)
        .await;

        if let Err(e) = insert_run_result {
             tracing::error!(%sim_id, "Failed to insert record into simulation_runs table: {}", e);
             // Continue anyway, but the history record will be missing
        }

        // 3. Update cost_history table *only* if simulation completed successfully
        if finished_sim.status == "Completed" {
            if let Some(actual_cost) = final_actual_cost {
                 let update_cost_hist_result = sqlx::query(
                    r#"
                    INSERT INTO cost_history (chart, node_count, duration_secs, cpu_cores, memory_gb, observed_at)
                    VALUES (?, ?, ?, ?, ?, ?)
                    ON CONFLICT(chart, node_count, duration_secs) DO UPDATE SET
                        cpu_cores = excluded.cpu_cores,
                        memory_gb = excluded.memory_gb,
                        observed_at = excluded.observed_at
                    "#
                )
                .bind(&finished_sim.params.chart)
                .bind(finished_sim.params.node_count as i64)
                .bind(finished_sim.params.duration_secs as i64)
                .bind(actual_cost.cpu_cores as f64)
                .bind(actual_cost.memory_gb as f64)
                .bind(end_time)
                .execute(db_pool)
                .await;

                if let Err(e) = update_cost_hist_result {
                     tracing::warn!(%sim_id, "Failed to update cost_history table: {}", e);
                }
            } else {
                 tracing::warn!(%sim_id, "Simulation completed but no final actual cost available to update cost_history.");
            }
        }

        tracing::info!(%sim_id, status=%final_status, "Simulation finished, removed from active, DB updated.");
        Some(finished_sim)

    } else {
        tracing::warn!(%sim_id, "Tried to finalize simulation but it wasn't in the active list.");
        None
    }
}

// Helper for cleanup when a simulation fails *before* running its duration
// Updated signature to take ActiveSimulation object
async fn cleanup_failed_simulation(state: &AppState, sim_to_clean: ActiveSimulation, release_name: &str, namespace: &str) {
    // Final status should already be set (e.g., Failed (Helm), Failed (Rollout))
    // Pass the current sim state to the main cleanup function
    let _ = cleanup_simulation(state, sim_to_clean, release_name, namespace, "Failed").await; // Use a generic failed status if not already specific
}

// ... rest of the file ...

// Handler to rerun a previous simulation
#[debug_handler]
async fn rerun_simulation_handler(
    State(state): State<AppState>,
    Path(simulation_id_str): Path<String>,
) -> impl IntoResponse {
    tracing::info!(%simulation_id_str, "Received request to rerun simulation");

    // Validate UUID format (optional but good practice)
    let simulation_id_to_rerun = match Uuid::parse_str(&simulation_id_str) {
        Ok(uuid) => uuid,
        Err(_) => {
            tracing::warn!(%simulation_id_str, "Invalid UUID format for rerun request");
            return (StatusCode::BAD_REQUEST, Json("Invalid simulation ID format")).into_response();
        }
    };

    // 1. Fetch the historical run details from DB
    let history_entry_result: Result<Option<SimulationRunHistoryEntry>, sqlx::Error> = 
        sqlx::query_as("SELECT * FROM simulation_runs WHERE simulation_id = ?")
        .bind(&simulation_id_str)
        .fetch_optional(&state.db_pool)
        .await;

    let history_entry = match history_entry_result {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            tracing::warn!(%simulation_id_str, "Simulation ID not found in history for rerun");
            return (StatusCode::NOT_FOUND, Json("Simulation ID not found in history")).into_response();
        }
        Err(e) => {
            tracing::error!(%simulation_id_str, "Database error fetching history for rerun: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, Json("Database error")).into_response();
        }
    };

    // 2. Deserialize the config_details JSON
    // We expect config_details to contain a serialized ActiveSimulation
    let original_sim_config: ActiveSimulation = match serde_json::from_str(&history_entry.config_details) {
        Ok(config) => config,
        Err(e) => {
             tracing::error!(%simulation_id_str, "Failed to deserialize config_details for rerun: {}", e);
             return (StatusCode::INTERNAL_SERVER_ERROR, Json("Failed to parse original simulation config")).into_response();
        }
    };

    // 3. Create a new QueuedSimulation based on the original config
    let new_request_id = Uuid::new_v4();
    let params = original_sim_config.params; // Reuse original params

    // Predict cost for the rerun (conditions might have changed)
    let predicted_cost = match predict_cost(&state.db_pool, &params).await {
        Ok(cost) => cost,
        Err(e) => {
             tracing::error!(%simulation_id_str, "Failed to predict cost for rerun: {}", e);
             // Proceed with a default prediction? Or fail the request?
             // Let's fail for now, as cost prediction is important.
             return (StatusCode::INTERNAL_SERVER_ERROR, Json("Failed to predict cost for rerun")).into_response();
        }
    };

    let new_queued_sim = QueuedSimulation {
        request_id: new_request_id,
        params,
        predicted_cost,
        issue_number: original_sim_config.issue_number, // Link to original issue if present
        docker_image: original_sim_config.docker_image,
        pubsub_topic: original_sim_config.pubsub_topic,
        bootstrap_nodes: Some(original_sim_config.bootstrap_nodes), // Assuming ActiveSim has this field correctly
        publisher_enabled: original_sim_config.publisher_enabled,
        publisher_message_size: original_sim_config.publisher_message_size,
        publisher_delay: original_sim_config.publisher_delay,
        publisher_message_count: original_sim_config.publisher_message_count,
        artificial_latency: original_sim_config.artificial_latency,
        latency_ms: original_sim_config.latency_ms,
        nodes_command: original_sim_config.nodes_command,
        bootstrap_command: original_sim_config.bootstrap_command,
        peer_number: original_sim_config.peer_number,
        number_of_peers: original_sim_config.number_of_peers,
        peers_to_connect: original_sim_config.peers_to_connect,
        message_rate: original_sim_config.message_rate,
        message_size: original_sim_config.message_size,
        // Use parallel limit from original config if possible, else default
        // Need to ensure ActiveSimulation stores this, or derive it somehow.
        // For now, defaulting to 1. TODO: Persist/retrieve parallel_limit
        parallel_limit: 1, 
    };

    // 4. Add to queue
    state.queued_simulations.lock().await.push_back(new_queued_sim.clone());

    // 5. Broadcast queue update
    let current_queue: Vec<QueuedSimulation> = state.queued_simulations.lock().await.iter().cloned().collect();
    let _ = state.event_sender.send(AppEvent::QueueUpdated(current_queue));

    tracing::info!(%simulation_id_str, new_request_id=%new_request_id, "Successfully queued simulation for rerun");

    // 6. Return success
    #[derive(Serialize)]
    struct RerunResponse { message: String, new_request_id: String }
    (StatusCode::OK, Json(RerunResponse { 
        message: "Simulation queued for rerun".to_string(),
        new_request_id: new_request_id.to_string()
    })).into_response()
}


// --- Main Application ---

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ... (dotenv, tracing, config loading, db setup) ...

    // --- Initialize Application State ---
    // ... (state initialization) ...

    // --- Start Background Tasks ---
    // ... (k8s, github, scheduler, monitor tasks) ...

    // --- Define Web Server Routes ---
    let app = Router::new()
        // Existing routes for UI, API, SSE
        .route("/",
            #[cfg(debug_assertions)]
            get(root_handler_debug),
            #[cfg(not(debug_assertions))]
            get(root_handler_release)
        )
        .route("/history",
            #[cfg(debug_assertions)]
            get(history_handler_debug),
            #[cfg(not(debug_assertions))]
            get(history_handler_release)
        )
        .route("/status-stream", get(sse_handler))
        .nest_service("/static", ServeDir::new("static"))
        // Existing API routes
        .route("/api/history", get(api_history_handler))
        .route("/mock_submit", post(mock_submit_handler)) // If still used
        .route("/set_time_dilation", post(set_time_dilation_handler)) // If still used
        // External Integration API (kept for potential direct interaction)
        .route("/api/v1/request_run", post(request_run_handler))
        .route("/api/v1/report_start", post(report_start_handler))
        .route("/api/v1/report_complete", post(report_complete_handler))
        // Measurement endpoint (if used by monitoring)
        // .route("/measure_usage", post(measure_simulation_usage))
        // Rerun Endpoint (NEW)
        .route("/api/rerun/:simulation_id", post(rerun_simulation_handler))
        // Add other routes as needed
        .with_state(AppState {
            templates: Arc::new(Mutex::new(AutoReloader::new(|_| {
                let mut env = minijinja::Environment::new();
                env.set_loader(path_loader("templates"));
                Ok(env)
            }))),
            queued_simulations: Arc::new(Mutex::new(VecDeque::new())),
            active_simulations: Arc::new(Mutex::new(HashMap::new())),
            last_finished_simulation: Arc::new(Mutex::new(None)),
            cluster_utilization: Arc::new(Mutex::new(ClusterUtilization::default())),
            namespace_utilization: Arc::new(Mutex::new(NamespaceUtilization::default())),
            db_pool: SqlitePool::connect("sqlite::memory:").await.unwrap(), // Use a real DB path later
            event_sender: broadcast::channel(100).0,
            scheduler_state: Arc::new(Mutex::new(SchedulerState::default())),
            time_dilation: Arc::new(Mutex::new(1)),
            pending_simulations: Arc::new(Mutex::new(HashMap::new())),
            github_token: "".to_string(), // Load from env/config
            github_repo: "".to_string(), // Load from env/config
            github_authorized_users: Vec::new(), // Load from env/config
            http_client: reqwest::Client::new(),
        })
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        );

    // Conditionally add Live Reload layer for debug builds
    #[cfg(feature = "debug")]
    let app = app.layer(LiveReloadLayer::new());

    // --- Run Server ---
    let addr = SocketAddr::from(([0, 0, 0, 0], 9930));
    tracing::info!("Listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// ... (rest of the file: background tasks, helpers, etc.) ...

// Helper to update labels after analysis attempt
async fn update_github_labels_post_analysis(state: &AppState, issue_number: u64, sim_id: Uuid, success: bool) {
    let issue_str = format!("#{}", issue_number);
    
    // 1. Remove "simulation-pending" label first
    if let Err(e) = remove_github_label(state, issue_number, "simulation-pending").await {
        tracing::warn!(%sim_id, issue = %issue_str, "Failed to remove 'simulation-pending' label (may not have existed): {}", e);
    }
    
    // 2. Add the final status label
    let final_label = if success { "simulation-done" } else { "simulation-failed" };
    if let Err(e) = add_github_label(state, issue_number, final_label).await {
        tracing::error!(%sim_id, issue = %issue_str, "Failed to add '{}' label: {}", final_label, e);
    } else {
        tracing::info!(%sim_id, issue = %issue_str, label=%final_label, "Added final status label.");
    }
    
    // Optionally remove "needs-scheduling" if it's still present?
    // if success {
    //     if let Err(e) = remove_github_label(state, issue_number, "needs-scheduling").await {
    //         tracing::warn!(%sim_id, issue = %issue_str, "Failed to remove 'needs-scheduling' label: {}", e);
    //     }
    // }
}

// --- Stubs for missing functions ---

// Stub for predict_cost
async fn predict_cost(db_pool: &SqlitePool, params: &SimulationParams) -> Result<ResourceCost, String> {
    tracing::warn!("Using STUB for predict_cost");
    // Basic fallback prediction
    let default_cpu_per_node = if params.chart == "waku" { 0.1 } else { 0.08 };
    let default_mem_per_node = if params.chart == "waku" { 0.05 } else { 0.04 };
    Ok(ResourceCost {
        cpu_cores: params.node_count as f32 * default_cpu_per_node,
        memory_gb: params.node_count as f32 * default_mem_per_node,
        monetary_cost_eur: None,
    })
}

// Stub for update_simulation_status
async fn update_simulation_status(state: &AppState, sim_id: Uuid, status: &str) {
    tracing::info!(%sim_id, %status, "STUB: update_simulation_status called");
    if let Some(sim) = state.active_simulations.lock().await.get_mut(&sim_id) {
        sim.status = status.to_string();
        // Broadcast update? Should probably happen here.
    }
}

// Stub for generate_values_yaml
fn generate_values_yaml(sim: &ActiveSimulation) -> Result<String, String> {
    tracing::warn!(%sim.simulation_id, "Using STUB for generate_values_yaml");
    Ok("key: value # Stub YAML".to_string())
}

// Stub for write_temp_values_file
async fn write_temp_values_file(content: &str) -> Result<std::path::PathBuf, std::io::Error> {
    use tokio::io::AsyncWriteExt;
    tracing::warn!("Using STUB for write_temp_values_file");
    let temp_dir = std::env::temp_dir().join("lars_values");
    tokio::fs::create_dir_all(&temp_dir).await?;
    let file_path = temp_dir.join(format!("values-{}.yaml", Uuid::new_v4())); // Unique name
    let mut file = tokio::fs::File::create(&file_path).await?;
    file.write_all(content.as_bytes()).await?;
    Ok(file_path)
}

// Stub for run_helm_deploy
async fn run_helm_deploy(sim: &ActiveSimulation, release_name: &str, namespace: &str, values_path: &std::path::Path) -> Result<(), String> {
    tracing::warn!(%sim.simulation_id, %release_name, "Using STUB for run_helm_deploy");
    // Simulate some delay
    tokio::time::sleep(Duration::from_secs(2)).await;
    Ok(())
}

// Stub for run_kubectl_rollout
async fn run_kubectl_rollout(statefulset_name: &str, namespace: &str) -> Result<(), String> {
    tracing::warn!(%statefulset_name, "Using STUB for run_kubectl_rollout");
    // Simulate some delay
    tokio::time::sleep(Duration::from_secs(3)).await;
    Ok(())
}

// Stub for run_helm_uninstall
async fn run_helm_uninstall(release_name: &str, namespace: &str) -> Result<(), String> {
    tracing::warn!(%release_name, "Using STUB for run_helm_uninstall");
    // Simulate some delay
    tokio::time::sleep(Duration::from_secs(1)).await;
    Ok(())
}
// Stub for measure_simulation_usage (Needed if route is uncommented)
/*
async fn measure_simulation_usage(State(state): State<AppState>, Json(payload): Json<SomePayloadType>) -> impl IntoResponse {
    tracing::warn!("Using STUB for measure_simulation_usage");
    StatusCode::OK
}
*/
// --- End Stubs ---
