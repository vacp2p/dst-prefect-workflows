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
use urlencoding;

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
    pub usage_snapshots: Vec<ResourceSnapshot>,
    pub last_snapshot_time: chrono::DateTime<chrono::Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monetary_cost_eur_predicted: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monetary_cost_eur_actual: Option<f32>,
    pub release_name: String, // Release name for tracking deployments
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
    // Store predicted cost for runs awaiting start report
    pending_simulations: Arc<Mutex<HashMap<Uuid, ResourceCost>>>, 
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
        params,
        predicted_cost: predicted_cost.clone(), 
        actual_cost: actual_cost.clone(),     
        monetary_cost_eur_predicted: Some(calculate_monetary_cost(&predicted_cost, payload.duration_secs)),
        monetary_cost_eur_actual: Some(calculate_monetary_cost(&actual_cost, payload.duration_secs)),
        usage_snapshots: Vec::new(), 
        last_snapshot_time: chrono::Utc::now(),
        release_name: payload.release_name, // Real release name from the client
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

// Ensure these handlers are defined before main
async fn api_history_handler(State(state): State<AppState>) -> impl IntoResponse {
    // Actual implementation...
    let db_pool = &state.db_pool;
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
        Ok(entries) => Json(entries).into_response(),
        Err(err) => {
            tracing::error!("Failed to fetch history data: {}", err);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(Vec::<HistoryEntry>::new())).into_response()
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
    // Check current utilization first
    let utilization_guard = match state.cluster_utilization.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            tracing::warn!("Scheduler couldn't acquire utilization lock, will retry later");
            return Ok(false);
        }
    };
    
    let current_cpu_percent = utilization_guard.cpu_percent;
    let current_memory_percent = utilization_guard.memory_percent;
    
    // Define thresholds
    const MAX_CPU_PERCENT: f32 = 30.0; // Maximum cluster CPU utilization allowed
    const MAX_MEMORY_PERCENT: f32 = 80.0; // Maximum cluster memory utilization allowed
    
    // Drop utilization lock before further processing
    drop(utilization_guard);
    
    // Try to get a lock on the queue
    let mut queue_guard = match state.queued_simulations.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            tracing::warn!("Scheduler couldn't acquire queue lock, will retry later");
            return Ok(false);
        }
    };
    
    // Check if queue is empty
    if queue_guard.is_empty() {
        return Ok(false);
    }
    
    // Peek at the next simulation (don't remove yet)
    let next_sim = match queue_guard.front() {
        Some(sim) => sim,
        None => return Ok(false), // Queue was empty (shouldn't happen due to check above)
    };
    
    // Calculate the additional CPU and memory usage this simulation would add
    let cpu_cores_predicted = next_sim.predicted_cost.cpu_cores;
    let memory_gb_predicted = next_sim.predicted_cost.memory_gb;
    
    // Get current cluster capacity
    let cpu_capacity = 1812.0; // From logs (replace with actual capacity from state if available)
    let memory_capacity = 2976.7385; // From logs (replace with actual capacity from state if available)
    
    // Calculate the additional percentage this would add
    let additional_cpu_percent = (cpu_cores_predicted / cpu_capacity) * 100.0;
    let additional_memory_percent = (memory_gb_predicted / memory_capacity) * 100.0;
    
    // Calculate new projected utilization
    let projected_cpu_percent = current_cpu_percent + additional_cpu_percent;
    let projected_memory_percent = current_memory_percent + additional_memory_percent;
    
    // Check if adding this simulation would exceed thresholds
    if projected_cpu_percent > MAX_CPU_PERCENT {
        tracing::info!(
            simulation_id = %next_sim.request_id,
            current_cpu = %current_cpu_percent,
            additional_cpu = %additional_cpu_percent,
            projected_cpu = %projected_cpu_percent,
            threshold = %MAX_CPU_PERCENT,
            "Simulation would exceed CPU threshold, keeping in queue"
        );
        return Ok(false);
    }
    
    if projected_memory_percent > MAX_MEMORY_PERCENT {
        tracing::info!(
            simulation_id = %next_sim.request_id,
            current_memory = %current_memory_percent,
            additional_memory = %additional_memory_percent,
            projected_memory = %projected_memory_percent,
            threshold = %MAX_MEMORY_PERCENT,
            "Simulation would exceed memory threshold, keeping in queue"
        );
        return Ok(false);
    }
    
    // Now that we've checked thresholds, we can pop the simulation from the queue
    let next_sim = queue_guard.pop_front().unwrap(); // Safe because we checked it exists
    
    tracing::info!(
        simulation_id = %next_sim.request_id,
        current_cpu = %current_cpu_percent,
        additional_cpu = %additional_cpu_percent,
        projected_cpu = %projected_cpu_percent,
        current_memory = %current_memory_percent,
        additional_memory = %additional_memory_percent,
        projected_memory = %projected_memory_percent,
        "Moving simulation from queue to active status"
    );
    
    // Now get lock on active simulations
    let mut active_guard = match state.active_simulations.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            // Put the simulation back in the queue
            queue_guard.push_front(next_sim);
            tracing::warn!("Scheduler couldn't acquire active simulations lock, returning simulation to queue");
            return Ok(false);
        }
    };
    
    // Convert QueuedSimulation to ActiveSimulation
    let active_sim = ActiveSimulation {
        simulation_id: next_sim.request_id,
        params: next_sim.params.clone(),
        predicted_cost: next_sim.predicted_cost.clone(),
        actual_cost: next_sim.predicted_cost.clone(), // Initialize with predicted
        usage_snapshots: Vec::new(),
        last_snapshot_time: chrono::Utc::now(),
        monetary_cost_eur_predicted: Some(calculate_monetary_cost(
            &next_sim.predicted_cost,
            next_sim.params.duration_secs
        )),
        monetary_cost_eur_actual: Some(calculate_monetary_cost(
            &next_sim.predicted_cost,
            next_sim.params.duration_secs
        )),
        release_name: format!("mock-{}-{}", next_sim.params.chart, next_sim.params.node_count), // Mark as mock
    };
    
    // Add to active simulations
    active_guard.insert(next_sim.request_id, active_sim);
    
    // Broadcast updates
    let queue_updated: Vec<QueuedSimulation> = queue_guard.iter().cloned().collect();
    let active_updated: Vec<ActiveSimulation> = active_guard.values().cloned().collect();
    
    // Release locks before broadcast to avoid deadlock
    drop(queue_guard);
    drop(active_guard);
    
    // Send the updates
    if let Err(e) = state.event_sender.send(AppEvent::QueueUpdated(queue_updated)) {
        tracing::error!("Failed to broadcast queue update: {}", e);
    }
    
    if let Err(e) = state.event_sender.send(AppEvent::ActiveUpdated(active_updated)) {
        tracing::error!("Failed to broadcast active update: {}", e);
    }
    
    // Successfully processed a simulation
    Ok(true)
}

async fn update_utilization_from_k8s(state: AppState) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let namespace = "larstesting";
    
    match fetch_prometheus_metrics(namespace).await {
        Ok(metrics) => {
            // Get active simulations costs
            let active_sims = state.active_simulations.lock().await;
            let sim_cpu: f32 = active_sims.values().map(|sim| sim.actual_cost.cpu_cores).sum();
            let sim_memory: f32 = active_sims.values().map(|sim| sim.actual_cost.memory_gb).sum();
            let active_sims_len = active_sims.len(); // Get length before dropping
            drop(active_sims);
            
            // Update cluster utilization using Prometheus base + simulation costs
            let mut cluster_util = state.cluster_utilization.lock().await;
            cluster_util.total_cpu_cores = metrics.cluster_total_cpu;
            cluster_util.total_memory_gb = metrics.cluster_total_memory_gb;
            cluster_util.used_cpu_cores = metrics.cluster_used_cpu + sim_cpu;
            cluster_util.used_memory_gb = metrics.cluster_used_memory_gb + sim_memory;
            cluster_util.cpu_percent = (cluster_util.used_cpu_cores / cluster_util.total_cpu_cores).max(0.0).min(100.0) * 100.0; // Clamp percentage
            cluster_util.memory_percent = (cluster_util.used_memory_gb / cluster_util.total_memory_gb).max(0.0).min(100.0) * 100.0; // Clamp percentage
            let cluster_util_clone = cluster_util.clone(); // Clone for broadcast
            drop(cluster_util);
            
            // Update namespace utilization similarly (if needed for UI, otherwise remove)
            // ... (optional: update namespace_util based on metrics.namespace_... + sim_...)

            // Broadcast updates
            let current_active = state.active_simulations.lock().await.values().cloned().collect(); // Re-lock is fine here
            let _ = state.event_sender.send(AppEvent::ActiveUpdated(current_active));
            let cpu_p = cluster_util_clone.cpu_percent; // Get value before potential move
            let mem_p = cluster_util_clone.memory_percent; // Get value before potential move
            let _ = state.event_sender.send(AppEvent::ClusterUtilizationUpdated(cluster_util_clone));
            
            tracing::debug!(
                "Resource update (Prometheus + Sims) - Cluster CPU: {:.1}%, Mem: {:.1}%, Total Active Sims: {}", 
                cpu_p, mem_p, active_sims_len // Use stored values
            );
            Ok(())
        },
        Err(e) => { // Correctly format the Err arm
            tracing::warn!("Prometheus fetch failed: {}. Falling back to simulation-only metrics.", e);
            
            // Fallback logic: Calculate utilization based ONLY on active simulations
            let active_sims = state.active_simulations.lock().await;
            let active_sims_len = active_sims.len();
            if active_sims_len > 0 {
                let sim_cpu: f32 = active_sims.values().map(|sim| sim.actual_cost.cpu_cores).sum();
                let sim_memory: f32 = active_sims.values().map(|sim| sim.actual_cost.memory_gb).sum();
                drop(active_sims);

                let mut cluster_util = state.cluster_utilization.lock().await;
                // Use previous total capacity if known, otherwise keep defaults
                // Update *used* based only on sims
                cluster_util.used_cpu_cores = sim_cpu;
                cluster_util.used_memory_gb = sim_memory;
                cluster_util.cpu_percent = (cluster_util.used_cpu_cores / cluster_util.total_cpu_cores).max(0.0).min(100.0) * 100.0; 
                cluster_util.memory_percent = (cluster_util.used_memory_gb / cluster_util.total_memory_gb).max(0.0).min(100.0) * 100.0;
                let cluster_util_clone = cluster_util.clone();
                drop(cluster_util);

                // Broadcast updates (simulation only)
                let current_active = state.active_simulations.lock().await.values().cloned().collect();
                let _ = state.event_sender.send(AppEvent::ActiveUpdated(current_active));
                // Get values before move
                let cpu_p_fallback = cluster_util_clone.cpu_percent;
                let mem_p_fallback = cluster_util_clone.memory_percent;
                let _ = state.event_sender.send(AppEvent::ClusterUtilizationUpdated(cluster_util_clone)); 
                
                tracing::debug!(
                    "Resource update (Sims Only Fallback) - CPU: {:.1}%, Mem: {:.1}%, Total Active Sims: {}", 
                    cpu_p_fallback, mem_p_fallback, active_sims_len // Use stored values
                );
            } else {
                drop(active_sims);
                // No active sims and Prometheus failed - maybe reset utilization?
                tracing::debug!("Prometheus failed and no active simulations. Utilization not updated.");
                // Optionally reset cluster_util here if desired
            }
            // Return Ok even in fallback, as the function itself didn't fail, just the source
            Ok(())
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
    if !response.status().is_success() {
        return Err(format!("Prometheus request failed for cluster CPU: {}", response.status()).into());
    }
    let prom_response: PrometheusResponse = response.json().await?;
    
    if prom_response.status == "success" && !prom_response.data.result.is_empty() {
        if let Ok(value) = prom_response.data.result[0].value.1.parse::<f32>() {
            metrics.cluster_total_cpu = value;
            tracing::debug!("Prometheus: Cluster CPU capacity: {}", value);
        }
    }
    
    // Fetch total cluster memory capacity (in GB)
    let cluster_mem_query = format!("{}query?query={}", prometheus_url, 
        "sum(kube_node_status_capacity{resource=\"memory\"}) / 1024 / 1024 / 1024");
    
    let response = http_client.get(&cluster_mem_query).send().await?;
    if !response.status().is_success() {
        return Err(format!("Prometheus request failed for cluster Memory: {}", response.status()).into());
    }
    let prom_response: PrometheusResponse = response.json().await?;
    
    if prom_response.status == "success" && !prom_response.data.result.is_empty() {
        if let Ok(value) = prom_response.data.result[0].value.1.parse::<f32>() {
            metrics.cluster_total_memory_gb = value;
            tracing::debug!("Prometheus: Cluster memory capacity: {} GB", value);
        }
    }
    
    // Fetch cluster CPU usage
    let cluster_cpu_usage_query = format!("{}query?query={}", prometheus_url, 
        "sum(rate(container_cpu_usage_seconds_total[5m]))");
    
    let response = http_client.get(&cluster_cpu_usage_query).send().await?;
     if !response.status().is_success() {
        return Err(format!("Prometheus request failed for cluster CPU usage: {}", response.status()).into());
    }
    let prom_response: PrometheusResponse = response.json().await?;
    
    if prom_response.status == "success" && !prom_response.data.result.is_empty() {
        if let Ok(value) = prom_response.data.result[0].value.1.parse::<f32>() {
            metrics.cluster_used_cpu = value;
            tracing::debug!("Prometheus: Cluster CPU usage: {}", value);
        }
    }
    
    // Fetch cluster memory usage (in GB)
    let cluster_mem_usage_query = format!("{}query?query={}", prometheus_url, 
        "sum(container_memory_working_set_bytes) / 1024 / 1024 / 1024");
    
    let response = http_client.get(&cluster_mem_usage_query).send().await?;
    if !response.status().is_success() {
        return Err(format!("Prometheus request failed for cluster Memory usage: {}", response.status()).into());
    }
    let prom_response: PrometheusResponse = response.json().await?;
    
    if prom_response.status == "success" && !prom_response.data.result.is_empty() {
        if let Ok(value) = prom_response.data.result[0].value.1.parse::<f32>() {
            metrics.cluster_used_memory_gb = value;
            tracing::debug!("Prometheus: Cluster memory usage: {} GB", value);
        }
    }
    
    // Fetch namespace CPU usage
    let namespace_cpu_query = format!("{}query?query={}", prometheus_url, 
        format!("sum(rate(container_cpu_usage_seconds_total{{namespace=\"{}\"}}[5m]))", namespace));
    
    let response = http_client.get(&namespace_cpu_query).send().await?;
    if !response.status().is_success() {
        return Err(format!("Prometheus request failed for namespace CPU usage: {}", response.status()).into());
    }
    let prom_response: PrometheusResponse = response.json().await?;
    
    if prom_response.status == "success" && !prom_response.data.result.is_empty() {
        if let Ok(value) = prom_response.data.result[0].value.1.parse::<f32>() {
            metrics.namespace_used_cpu = value;
            tracing::debug!("Prometheus: Namespace {} CPU usage: {}", namespace, value);
        }
    }
    
    // Fetch namespace memory usage (in GB)
    let namespace_mem_query = format!("{}query?query={}", prometheus_url, 
        format!("sum(container_memory_working_set_bytes{{namespace=\"{}\"}}) / 1024 / 1024 / 1024", namespace));
    
    let response = http_client.get(&namespace_mem_query).send().await?;
    if !response.status().is_success() {
        return Err(format!("Prometheus request failed for namespace Memory usage: {}", response.status()).into());
    }
    let prom_response: PrometheusResponse = response.json().await?;
    
    if prom_response.status == "success" && !prom_response.data.result.is_empty() {
        if let Ok(value) = prom_response.data.result[0].value.1.parse::<f32>() {
            metrics.namespace_used_memory_gb = value;
            tracing::debug!("Prometheus: Namespace {} memory usage: {} GB", namespace, value);
        }
    }
    
    Ok(metrics)
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MeasureUsagePayload {
    simulation_id: Uuid,
    release_name: String,
    namespace: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MeasureUsageResponse {
    simulation_id: Uuid,
    cpu_cores: f32,
    memory_gb: f32,
    success: bool,
    message: Option<String>,
}

#[debug_handler]
async fn measure_simulation_usage(
    State(state): State<AppState>,
    Json(payload): Json<MeasureUsagePayload>,
) -> impl IntoResponse {
    tracing::info!("Measuring actual usage for simulation {}: in namespace {}, release {}",
                  payload.simulation_id, payload.namespace, payload.release_name);
    
    // Check if this simulation is active
    let sim_id = payload.simulation_id;
    let mut active_guard = match state.active_simulations.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            return (StatusCode::SERVICE_UNAVAILABLE, Json(MeasureUsageResponse {
                simulation_id: sim_id,
                cpu_cores: 0.0,
                memory_gb: 0.0,
                success: false,
                message: Some("Could not acquire lock on active simulations".to_string()),
            })).into_response();
        }
    };
    
    let active_sim = match active_guard.get_mut(&sim_id) {
        Some(sim) => sim,
        None => {
            drop(active_guard);
            return (StatusCode::NOT_FOUND, Json(MeasureUsageResponse {
                simulation_id: sim_id,
                cpu_cores: 0.0,
                memory_gb: 0.0,
                success: false,
                message: Some("Simulation not found in active simulations".to_string()),
            })).into_response();
        }
    };
    
    // Use Prometheus to get actual resource usage
    let usage_result = match fetch_simulation_usage(&payload.namespace, &payload.release_name).await {
        Ok(usage) => {
            // Update the simulation with actual usage
            active_sim.actual_cost.cpu_cores = usage.cpu_cores;
            active_sim.actual_cost.memory_gb = usage.memory_gb;
            
            // Add a new snapshot of resource usage
            active_sim.usage_snapshots.push(ResourceSnapshot {
                timestamp: chrono::Utc::now(),
                cpu_cores: usage.cpu_cores,
                memory_gb: usage.memory_gb,
            });
            active_sim.last_snapshot_time = chrono::Utc::now();
            
            // Calculate monetary cost based on actual usage
            active_sim.monetary_cost_eur_actual = Some(calculate_monetary_cost(
                &active_sim.actual_cost,
                active_sim.params.duration_secs
            ));
            
            MeasureUsageResponse {
                simulation_id: sim_id,
                cpu_cores: usage.cpu_cores,
                memory_gb: usage.memory_gb,
                success: true,
                message: None,
            }
        },
        Err(e) => {
            tracing::warn!("Failed to measure actual usage for simulation {}: {}", sim_id, e);
            MeasureUsageResponse {
                simulation_id: sim_id,
                cpu_cores: active_sim.actual_cost.cpu_cores, // Return current values
                memory_gb: active_sim.actual_cost.memory_gb,
                success: false,
                message: Some(format!("Failed to measure usage: {}", e)),
            }
        }
    };
    
    // Get a copy of active simulations for broadcasting
    let active_sims = active_guard.values().cloned().collect::<Vec<ActiveSimulation>>();
    drop(active_guard);
    
    // Broadcast active simulation update
    let _ = state.event_sender.send(AppEvent::ActiveUpdated(active_sims));
    
    Json(usage_result).into_response()
}

// Helper struct for simulation usage
struct SimulationUsage {
    cpu_cores: f32,
    memory_gb: f32,
}

// Fetch the resource usage for a specific simulation release from Prometheus
async fn fetch_simulation_usage(namespace: &str, release_name: &str) -> Result<SimulationUsage, Box<dyn std::error::Error + Send + Sync>> {
    let prometheus_url = "https://metrics.riff.cc/select/0/prometheus/api/v1/";
    let http_client = reqwest::Client::new();
    
    // Log the original release name for debugging
    tracing::debug!("Fetching resource usage for: namespace={}, release_name={}", namespace, release_name);
    
    // More robust release name escaping for Prometheus regex
    // Escape special regex characters: ., +, *, ?, ^, $, (, ), [, ], {, }, |, \
    let release_pattern = release_name
        .replace(".", "\\\\.")
        .replace("+", "\\\\+")
        .replace("*", "\\\\*")
        .replace("?", "\\\\?")
        .replace("^", "\\\\^")
        .replace("$", "\\\\$")
        .replace("(", "\\\\(")
        .replace(")", "\\\\)")
        .replace("[", "\\\\[")
        .replace("]", "\\\\]")
        .replace("{", "\\\\{")
        .replace("}", "\\\\}")
        .replace("|", "\\\\|")
        .replace("\\", "\\\\\\\\");
    
    // Use a more precise regex pattern to match only the pods for this release
    // This pattern matches pods that start with the release name followed by a dash
    // and then anything, e.g., "release-name-*"
    let pod_pattern = format!("^{}(-|$)", release_pattern);
    
    // CPU query: Get the sum of CPU usage for all pods with this release name in the namespace
    let cpu_query = format!(
        "sum(rate(container_cpu_usage_seconds_total{{namespace=\"{}\", pod=~\"{}.*\"}}[1m]))",
        namespace, pod_pattern
    );
    
    // Log the constructed Prometheus query for debugging
    tracing::debug!("Prometheus CPU query: {}", cpu_query);
    
    let cpu_query_url = format!("{}query?query={}", prometheus_url, urlencoding::encode(&cpu_query));
    
    let response = http_client.get(&cpu_query_url).send().await?;
    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_else(|_| "Could not extract error text".to_string());
        return Err(format!("Prometheus request failed for CPU: {} - {}", status, error_text).into());
    }
    
    let prom_response: PrometheusResponse = response.json().await?;
    let cpu_cores = if prom_response.status == "success" && !prom_response.data.result.is_empty() {
        prom_response.data.result[0].value.1.parse::<f32>().unwrap_or(0.0)
    } else {
        tracing::warn!("No CPU data found for namespace={}, release_name={}", namespace, release_name);
        0.0
    };
    
    // Memory query: Get the sum of memory usage for all pods with this release name in the namespace
    let memory_query = format!(
        "sum(container_memory_working_set_bytes{{namespace=\"{}\", pod=~\"{}.*\"}}) / (1024*1024*1024)",
        namespace, pod_pattern
    );
    
    // Log the constructed Prometheus query for debugging
    tracing::debug!("Prometheus Memory query: {}", memory_query);
    
    let memory_query_url = format!("{}query?query={}", prometheus_url, urlencoding::encode(&memory_query));
    
    let response = http_client.get(&memory_query_url).send().await?;
    if !response.status().is_success() {
        let status = response.status();
        let error_text = response.text().await.unwrap_or_else(|_| "Could not extract error text".to_string());
        return Err(format!("Prometheus request failed for Memory: {} - {}", status, error_text).into());
    }
    
    let prom_response: PrometheusResponse = response.json().await?;
    let memory_gb = if prom_response.status == "success" && !prom_response.data.result.is_empty() {
        prom_response.data.result[0].value.1.parse::<f32>().unwrap_or(0.0)
    } else {
        tracing::warn!("No Memory data found for namespace={}, release_name={}", namespace, release_name);
        0.0
    };
    
    tracing::info!(
        "Measured actual usage for {}/{}: CPU cores: {:.2}, Memory GB: {:.2}",
        namespace, release_name, cpu_cores, memory_gb
    );
    
    Ok(SimulationUsage {
        cpu_cores,
        memory_gb,
    })
}

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
        env.set_loader(path_loader("templates"));
        notifier.watch_path("templates", true);
        Ok(env)
    });

    // --- Database Setup ---
    let db_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    tracing::debug!("Using database URL: {}", db_url);
    
    // Check if database exists and create it if it doesn't
    if !std::path::Path::new(&db_url).exists() {
        tracing::info!("Database file does not exist, will be created by SqlitePool");
    }
    
    // Connect to the database
    tracing::debug!("Connecting to SQLite database");
    let db_pool = SqlitePool::connect(&db_url).await.expect("Failed to connect to database");
    
    // Create tables if they don't exist
    tracing::debug!("Setting up database tables");
    let create_table_result = sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS cost_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            chart TEXT NOT NULL,
            node_count INTEGER NOT NULL,
            duration_secs INTEGER NOT NULL,
            cpu_cores REAL NOT NULL,
            memory_gb REAL NOT NULL,
            observed_at TEXT NOT NULL
        )
        "#
    ).execute(&db_pool).await;
    
    match create_table_result {
        Ok(_) => tracing::debug!("cost_history table created or already exists"),
        Err(e) => tracing::error!("Failed to create cost_history table: {}", e),
    }

    // --- Initial State Setup ---
    let (event_sender, _) = broadcast::channel::<AppEvent>(100);

    let state = AppState {
        templates: Arc::new(Mutex::new(templates_reloader)), // Now in scope
        queued_simulations: Arc::new(Mutex::new(VecDeque::new())),
        active_simulations: Arc::new(Mutex::new(HashMap::new())),
        last_finished_simulation: Arc::new(Mutex::new(None)),
        cluster_utilization: Arc::new(Mutex::new(ClusterUtilization::default())),
        namespace_utilization: Arc::new(Mutex::new(NamespaceUtilization::default())),
        db_pool, // Now in scope
        event_sender: event_sender.clone(), 
        scheduler_state: Arc::new(Mutex::new(SchedulerState::default())),
        time_dilation: Arc::new(Mutex::new(1)), 
        // Store predicted cost for runs awaiting start report
        pending_simulations: Arc::new(Mutex::new(HashMap::new())), 
    };

    // --- Mock Scheduler Task (remains) ---
    let scheduler_state_clone = state.clone();
    tokio::spawn(async move {
        let state = scheduler_state_clone; // Renamed for clarity
        
        // Scheduler loop - check queue and move to active status
        loop {
            let result = process_next_simulation_from_queue(&state).await;
            match result {
                Ok(true) => {
                    // Successfully processed a simulation, continue immediately to process more
                    tracing::debug!("Scheduler processed simulation from queue, checking for more...");
                    continue;
                }
                Ok(false) => {
                    // No simulations processed, wait for a bit
                    tracing::debug!("Scheduler found no simulations to process");
                }
                Err(e) => {
                    tracing::error!("Scheduler error processing simulation: {}", e);
                }
            }
            // Wait before checking again
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    });

    // --- Mock Monitoring Task (remains) ---
    let monitor_state_clone = state.clone();
    tokio::spawn(async move {
        let state = monitor_state_clone; // Renamed for clarity
        loop {
            // Update overall cluster utilization
            if let Err(e) = update_utilization_from_k8s(state.clone()).await {
                 tracing::warn!("Failed to update utilization from k8s: {:?}", e);
            }
            
            // Get active simulations and measure their actual resource usage
            {
                let active_guard = match state.active_simulations.try_lock() {
                    Ok(guard) => guard,
                    Err(_) => {
                        tracing::warn!("Monitoring task couldn't acquire active simulations lock, skipping resource measurement");
                        tokio::time::sleep(std::time::Duration::from_secs(25)).await;
                        continue;
                    }
                };
                
                // Skip if no active simulations
                if active_guard.is_empty() {
                    drop(active_guard);
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    continue;
                }
                
                let active_sim_count = active_guard.len();
                tracing::debug!("Monitoring task measuring resource usage for {} active simulations", active_sim_count);
                
                // First just collect the data we need while holding the lock briefly
                let sim_data: Vec<(Uuid, String, String)> = active_guard.iter()
                    .map(|(id, sim)| (*id, sim.params.chart.clone(), sim.release_name.clone()))
                    .collect();
                drop(active_guard);
                
                // Keep track of whether any simulations were updated
                let mut any_updated = false;
                
                // Measure each simulation's resource usage (outside the lock)
                for (sim_id, _chart, release_name) in sim_data {
                    // Skip mock simulations or those with invalid/placeholder release names
                    if release_name.starts_with("mock-") || release_name.contains("mock") {
                        tracing::debug!("Skipping resource measurement for mock simulation {}: release_name='{}'", sim_id, release_name);
                        continue;
                    }
                    
                    // Skip if release name doesn't look like a valid Kubernetes resource name
                    if !is_valid_release_name(&release_name) {
                        tracing::debug!("Skipping resource measurement for simulation {} with invalid release name: '{}'", sim_id, release_name);
                        continue;
                    }
                    
                    // Additional check: release name should be for a real deployment
                    // Most real Kubernetes deployments will have nodes-X, statefulset-X, or deployment-X patterns
                    if !release_name.contains("-nodes") && 
                       !release_name.contains("-deployment") && 
                       !release_name.contains("-statefulset") &&
                       !release_name.contains("-pod") {
                        // This is probably not a real deployment but we'll try anyway with a warning
                        tracing::warn!("Release name '{}' doesn't follow expected naming pattern for deployments, measurement may fail", release_name);
                    }
                    
                    // Use the stored release_name from the report_start request
                    match fetch_simulation_usage("larstesting", &release_name).await {
                        Ok(usage) => {
                            // Now get the lock again to update just this simulation
                            if let Ok(mut active_guard) = state.active_simulations.try_lock() {
                                if let Some(sim) = active_guard.get_mut(&sim_id) {
                                    // Update the simulation with actual usage
                                    sim.actual_cost.cpu_cores = usage.cpu_cores;
                                    sim.actual_cost.memory_gb = usage.memory_gb;
                                    
                                    // Add a new snapshot of resource usage
                                    sim.usage_snapshots.push(ResourceSnapshot {
                                        timestamp: chrono::Utc::now(),
                                        cpu_cores: usage.cpu_cores,
                                        memory_gb: usage.memory_gb,
                                    });
                                    sim.last_snapshot_time = chrono::Utc::now();
                                    
                                    // Calculate monetary cost based on actual usage
                                    sim.monetary_cost_eur_actual = Some(calculate_monetary_cost(
                                        &sim.actual_cost,
                                        sim.params.duration_secs
                                    ));
                                    
                                    any_updated = true;
                                    tracing::info!(
                                        %sim_id,
                                        "Updated actual resource usage. CPU: {:.2} cores, Memory: {:.2} GB", 
                                        usage.cpu_cores, usage.memory_gb
                                    );
                                }
                            }
                        },
                        Err(e) => {
                            tracing::warn!("Failed to measure resource usage for simulation {}: {}", sim_id, e);
                        }
                    }
                }
                
                // Broadcast an update if we modified any simulations
                if any_updated {
                    if let Ok(active_guard) = state.active_simulations.try_lock() {
                        let active_sims = active_guard.values().cloned().collect::<Vec<ActiveSimulation>>();
                        drop(active_guard);
                        let _ = state.event_sender.send(AppEvent::ActiveUpdated(active_sims));
                    }
                }
            }
            
            // Wait before checking again (every 30 seconds)
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });

    // --- Build Router AFTER State Initialization ---
    let mut app = Router::new()
        // Routes using handlers defined outside main
        .route("/",
            #[cfg(debug_assertions)]
            axum::routing::get(root_handler_debug),
            #[cfg(not(debug_assertions))]
            axum::routing::get(root_handler_release)
        )
        .route("/api/history", axum::routing::get(api_history_handler))
        .route("/status-stream", axum::routing::get(sse_handler))
        .route("/mock_submit", axum::routing::post(mock_submit_handler))
        .route("/set_time_dilation", axum::routing::post(set_time_dilation_handler))
        .route("/api/v1/request_run", axum::routing::post(request_run_handler))
        .route("/api/v1/report_start", axum::routing::post(report_start_handler))
        .route("/api/v1/report_complete", axum::routing::post(report_complete_handler))
        .route("/measure_usage", axum::routing::post(measure_simulation_usage))
        .nest_service("/static", ServeDir::new("static"))
        .route("/history",
            #[cfg(debug_assertions)]
            axum::routing::get(history_handler_debug),
            #[cfg(not(debug_assertions))]
            axum::routing::get(history_handler_release)
        )
        .with_state(state.clone())
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::default().include_headers(true)),
        );

    // Add Live Reload Layer conditionally
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

/// Check if a release name appears to be valid for Kubernetes
/// 
/// This doesn't need to be a perfect validation, just enough to filter out
/// mock or placeholder names that would cause 422 errors in Prometheus
fn is_valid_release_name(name: &str) -> bool {
    // Must not be empty
    if name.is_empty() {
        return false;
    }
    
    // Must not contain "mock" anywhere
    if name.to_lowercase().contains("mock") {
        return false;
    }
    
    // Must be at least a few characters long
    if name.len() < 3 {
        return false;
    }

    // Print the chart name/release name for debugging
    tracing::debug!("Checking release name: {}", name);
    
    // Basic chart name check - must start with either "waku-" or "nimlibp2p-"
    if !name.starts_with("waku-") && !name.starts_with("nimlibp2p-") {
        tracing::debug!("Release name doesn't start with a known chart prefix: {}", name);
        return false;
    }
    
    // Simplify validation - just ensure we don't have characters that would break Prometheus queries
    let has_risky_chars = name.contains("\\") || 
                          name.contains("\"") || 
                          name.contains("'");
    
    !has_risky_chars
}