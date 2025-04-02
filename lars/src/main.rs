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
use kube::{
    api::{Api, ListParams, ResourceExt},
    Client,
};
use k8s_openapi::api::core::v1::{Node, Pod};
use futures::TryStreamExt;

// --- Data Structures ---

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SimulationParams {
    pub chart: String,
    pub node_count: u32,
    pub duration_mins: u32,
}

impl SimulationParams {
    fn history_key(&self) -> String {
        format!("{}:{}", self.chart, self.node_count)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, FromRow)]
struct ResourceCost {
    #[sqlx(rename = "cpu_cores")]
    cpu_cores: f32,
    #[sqlx(rename = "memory_gb")]
    memory_gb: f32,
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

#[derive(Serialize, Clone, Debug)]
pub struct ActiveSimulation {
    pub simulation_id: Uuid,
    pub params: SimulationParams,
    pub predicted_cost: ResourceCost,
    pub actual_cost: ResourceCost,
}

// Add struct for history entries
#[derive(Serialize, FromRow, Debug, Clone)]
pub struct CostHistoryEntry {
    chart: String,
    node_count: u32,
    duration_mins: u32,
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
    duration_mins: i64,
    cpu_cores: f64,
    memory_gb: f64,
    observed_at: chrono::DateTime<chrono::Utc>,
}

// Structure to hold Kubernetes metrics
#[derive(Debug, Clone)]
struct KubernetesMetrics {
    // Cluster metrics
    cluster_total_cpu: f32,
    cluster_used_cpu: f32,
    cluster_total_memory_gb: f32,
    cluster_used_memory_gb: f32,
    
    // Namespace metrics (our simulations namespace)
    namespace: String,
    namespace_cpu_requests: f32,
    namespace_cpu_limits: f32,
    namespace_memory_requests_gb: f32,
    namespace_memory_limits_gb: f32,
    namespace_used_cpu: f32,
    namespace_used_memory_gb: f32,
}

impl Default for KubernetesMetrics {
    fn default() -> Self {
        Self {
            cluster_total_cpu: 1812.0,  // Default based on real cluster
            cluster_used_cpu: 0.0,
            cluster_total_memory_gb: 2978.0, // 2.91 TiB converted to GB (2.91 * 1024)
            cluster_used_memory_gb: 0.0,
            
            namespace: "larstesting".to_string(),
            namespace_cpu_requests: 0.0,
            namespace_cpu_limits: 256.0,  // Default max CPU for namespace
            namespace_memory_requests_gb: 0.0,
            namespace_memory_limits_gb: 1024.0, // Default max memory for namespace
            namespace_used_cpu: 0.0,
            namespace_used_memory_gb: 0.0,
        }
    }
}

// --- Application State ---

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
    let mut queue = state.queued_simulations.lock().await;
    let db_pool = &state.db_pool;
    let mut newly_predicted_costs = HashMap::new();

    for i in 0..payload.count {
        let node_count = payload.base_nodes + rand::thread_rng().gen_range(0..100) * (i + 1);
        let duration_mins = rand::thread_rng().gen_range(5..15);
        let params = SimulationParams {
            chart: format!("mock-chart-{}", rand::thread_rng().gen_range(1..4)),
            node_count,
            duration_mins,
        };

        // --- Predict cost using DB or default ---
        let predicted_cost_result: Result<Option<ResourceCost>, sqlx::Error> = sqlx::query_as(
            "SELECT cpu_cores, memory_gb FROM cost_history WHERE chart = ? AND node_count = ? ORDER BY observed_at DESC LIMIT 1"
        )
        .bind(&params.chart)
        .bind(params.node_count)
        .fetch_optional(db_pool)
        .await;

        let predicted_cost = match predicted_cost_result {
            Ok(Some(cost)) => {
                tracing::info!(chart = %params.chart, nodes = %params.node_count, cost = ?cost, "Found historical cost in DB");
                cost
            }
            Ok(None) => {
                // Create RNG only when needed
                let mut rng = rand::thread_rng();
                
                // Calculate more realistic resource requirements
                // For reference: 10,000 nodes would use about 1812 vCPUs (user provided info)
                // So approximately 0.18 CPU cores per node, with some variation
                let cpu_per_node = 0.18 + (rng.gen::<f32>() * 0.04 - 0.02); // 0.16-0.20 CPU per node
                let memory_per_node = 0.05 + (rng.gen::<f32>() * 0.02 - 0.01); // 0.04-0.06 GB per node
                
                let default_cost = ResourceCost {
                    cpu_cores: node_count as f32 * cpu_per_node,
                    memory_gb: node_count as f32 * memory_per_node,
                };
                tracing::info!(chart = %params.chart, nodes = %params.node_count, cost = ?default_cost, "No history found, using default predicted cost");
                newly_predicted_costs.insert((params.chart.clone(), params.node_count), default_cost.clone());
                default_cost
            }
            Err(e) => {
                tracing::error!(chart = %params.chart, nodes = %params.node_count, "DB error fetching cost: {}. Using default.", e);
                // Create RNG only when needed
                let mut rng = rand::thread_rng();
                
                // Same realistic CPU/memory calculations as above
                let cpu_per_node = 0.18 + (rng.gen::<f32>() * 0.04 - 0.02); // 0.16-0.20 CPU per node
                let memory_per_node = 0.05 + (rng.gen::<f32>() * 0.02 - 0.01); // 0.04-0.06 GB per node
                
                ResourceCost {
                    cpu_cores: node_count as f32 * cpu_per_node,
                    memory_gb: node_count as f32 * memory_per_node,
                }
            }
        };
        // -------------------------------------------

        let queued_sim = QueuedSimulation {
            request_id: Uuid::new_v4(),
            params,
            predicted_cost,
        };

        queue.push_back(queued_sim);
    }

    // Optionally: Persist newly predicted costs if desired (outside the loop for efficiency)
    // for ((chart, node_count), cost) in newly_predicted_costs { ... INSERT query ... }

    // Broadcast the queue update
    let _ = state.event_sender.send(AppEvent::QueueUpdated(queue.iter().cloned().collect()));
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
            duration_mins,
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
                duration_mins: row.get("duration_mins"),
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

// Function to fetch metrics from Kubernetes
async fn fetch_kubernetes_metrics(namespace: &str) -> Result<KubernetesMetrics, kube::Error> {
    // Initialize Kubernetes client
    let client = Client::try_default().await?;
    
    // Get all nodes to calculate cluster capacity
    let nodes_api: Api<Node> = Api::all(client.clone());
    let nodes = nodes_api.list(&ListParams::default()).await?;
    
    // Get pods in the specified namespace
    let pods_api: Api<Pod> = Api::namespaced(client, namespace);
    let pods = pods_api.list(&ListParams::default()).await?;
    
    // Initialize metrics with defaults
    let mut metrics = KubernetesMetrics::default();
    metrics.namespace = namespace.to_string();
    
    // Calculate cluster metrics from nodes
    for node in nodes.items {
        if let Some(status) = node.status {
            if let Some(allocatable) = status.allocatable {
                // Extract CPU and memory
                if let Some(cpu_str) = allocatable.get("cpu") {
                    if let Ok(cpu) = cpu_str.0.parse::<f32>() {
                        metrics.cluster_total_cpu += cpu;
                    }
                }
                
                if let Some(memory_str) = allocatable.get("memory") {
                    // Memory is typically in Ki, Mi, Gi format
                    // For simplicity, assume it's in GB, would need proper parsing
                    if memory_str.0.ends_with("Ki") {
                        if let Ok(mem) = memory_str.0.trim_end_matches("Ki").parse::<f32>() {
                            metrics.cluster_total_memory_gb += mem / (1024.0 * 1024.0);
                        }
                    } else if memory_str.0.ends_with("Mi") {
                        if let Ok(mem) = memory_str.0.trim_end_matches("Mi").parse::<f32>() {
                            metrics.cluster_total_memory_gb += mem / 1024.0;
                        }
                    } else if memory_str.0.ends_with("Gi") {
                        if let Ok(mem) = memory_str.0.trim_end_matches("Gi").parse::<f32>() {
                            metrics.cluster_total_memory_gb += mem;
                        }
                    }
                }
            }
        }
    }
    
    // Calculate namespace metrics from pods
    for pod in pods.items {
        if let Some(status) = pod.status {
            if status.phase.as_deref() == Some("Running") {
                if let Some(spec) = pod.spec {
                    for container in &spec.containers {
                        if let Some(resources) = &container.resources {
                            // Calculate CPU requests
                            if let Some(requests) = &resources.requests {
                                if let Some(cpu_str) = requests.get("cpu") {
                                    // CPU can be in cores or millicores
                                    let cpu_val = if cpu_str.0.ends_with("m") {
                                        if let Ok(cpu) = cpu_str.0.trim_end_matches("m").parse::<f32>() {
                                            cpu / 1000.0
                                        } else {
                                            0.0
                                        }
                                    } else {
                                        cpu_str.0.parse::<f32>().unwrap_or(0.0)
                                    };
                                    metrics.namespace_cpu_requests += cpu_val;
                                    // For simplicity, use requests as "used" since we can't get actual usage without metrics server
                                    metrics.namespace_used_cpu += cpu_val;
                                }
                                
                                if let Some(memory_str) = requests.get("memory") {
                                    // Memory parsing, similar to above
                                    if memory_str.0.ends_with("Ki") {
                                        if let Ok(mem) = memory_str.0.trim_end_matches("Ki").parse::<f32>() {
                                            let mem_gb = mem / (1024.0 * 1024.0);
                                            metrics.namespace_memory_requests_gb += mem_gb;
                                            metrics.namespace_used_memory_gb += mem_gb;
                                        }
                                    } else if memory_str.0.ends_with("Mi") {
                                        if let Ok(mem) = memory_str.0.trim_end_matches("Mi").parse::<f32>() {
                                            let mem_gb = mem / 1024.0;
                                            metrics.namespace_memory_requests_gb += mem_gb;
                                            metrics.namespace_used_memory_gb += mem_gb;
                                        }
                                    } else if memory_str.0.ends_with("Gi") {
                                        if let Ok(mem) = memory_str.0.trim_end_matches("Gi").parse::<f32>() {
                                            metrics.namespace_memory_requests_gb += mem;
                                            metrics.namespace_used_memory_gb += mem;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    
    // For cluster used metrics, we'll just simulate for demo purposes
    // In a real system, you'd get this from metrics-server or prometheus
    metrics.cluster_used_cpu = metrics.namespace_used_cpu + (metrics.cluster_total_cpu * 0.25); // 25% base + namespace
    metrics.cluster_used_memory_gb = metrics.namespace_used_memory_gb + (metrics.cluster_total_memory_gb * 0.3); // 30% base + namespace
    
    Ok(metrics)
}

// Function to update cluster and namespace utilization from Kubernetes metrics
async fn update_utilization_from_k8s(state: AppState) -> Result<(), kube::Error> {
    // Specify the simulation namespace
    let namespace = "larstesting";
    
    // Fetch metrics from Kubernetes
    match fetch_kubernetes_metrics(namespace).await {
        Ok(metrics) => {
            // Update cluster utilization
            let mut cluster_util = state.cluster_utilization.lock().await;
            cluster_util.total_cpu_cores = metrics.cluster_total_cpu;
            cluster_util.total_memory_gb = metrics.cluster_total_memory_gb;
            cluster_util.used_cpu_cores = metrics.cluster_used_cpu;
            cluster_util.used_memory_gb = metrics.cluster_used_memory_gb;
            cluster_util.cpu_percent = (metrics.cluster_used_cpu / metrics.cluster_total_cpu) * 100.0;
            cluster_util.memory_percent = (metrics.cluster_used_memory_gb / metrics.cluster_total_memory_gb) * 100.0;
            
            // Update namespace utilization
            let mut namespace_util = state.namespace_utilization.lock().await;
            namespace_util.allocated_cpu_cores = metrics.namespace_cpu_limits;
            namespace_util.allocated_memory_gb = metrics.namespace_memory_limits_gb;
            namespace_util.used_cpu_cores = metrics.namespace_used_cpu;
            namespace_util.used_memory_gb = metrics.namespace_used_memory_gb;
            namespace_util.cpu_percent = (namespace_util.used_cpu_cores / namespace_util.allocated_cpu_cores) * 100.0;
            namespace_util.memory_percent = (namespace_util.used_memory_gb / namespace_util.allocated_memory_gb) * 100.0;
            namespace_util.cluster_total_cpu = metrics.cluster_total_cpu;
            namespace_util.cluster_total_memory = metrics.cluster_total_memory_gb;
            
            // Send utilization updates
            let _ = state.event_sender.send(AppEvent::ClusterUtilizationUpdated(cluster_util.clone()));
            let _ = state.event_sender.send(AppEvent::NamespaceUtilizationUpdated(namespace_util.clone()));
            
            tracing::info!("Updated utilization metrics from Kubernetes for namespace '{}'", namespace);
            Ok(())
        },
        Err(e) => {
            tracing::error!("Failed to fetch Kubernetes metrics for namespace '{}': {}", namespace, e);
            Err(e)
        }
    }
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
    };

    // --- Mock Monitoring Task ---
    let monitor_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1)); // Update frequently
        loop {
            interval.tick().await;
            let mut active_sims = monitor_state.active_simulations.lock().await;
            let mut updated = false;

            if active_sims.len() > 0 {
                // Get resource limits
                let mut cluster_util = monitor_state.cluster_utilization.lock().await;
                let mut namespace_util = monitor_state.namespace_utilization.lock().await;
                
                // Extract limits
                let total_cluster_cpu = cluster_util.total_cpu_cores;
                let total_cluster_memory = cluster_util.total_memory_gb;
                let allocated_namespace_cpu = namespace_util.allocated_cpu_cores;
                let allocated_namespace_memory = namespace_util.allocated_memory_gb;
                
                // Mock overall cluster utilization (independent of our namespace)
                // This would come from external monitoring in a real system
                // For this mock, we'll simulate cluster having base load plus our namespace
                let base_cluster_cpu_usage = total_cluster_cpu * 0.25; // 25% base load from other namespaces
                let base_cluster_memory_usage = total_cluster_memory * 0.30; // 30% base load from other namespaces
                
                // Calculate total resource usage from our simulations
                let mut namespace_cpu = 0.0;
                let mut namespace_memory = 0.0;

                // Simulate cost changes for active simulations
                for sim in active_sims.values_mut() {
                    // Apply realistic variation with small jitter
                    let cpu_jitter = 0.85 + rand::random::<f32>() * 0.3; // 85% to 115% variation
                    sim.actual_cost.cpu_cores = sim.predicted_cost.cpu_cores * cpu_jitter;
                    sim.actual_cost.memory_gb = sim.predicted_cost.memory_gb * (0.9 + rand::random::<f32>() * 0.2);
                    
                    // Add to namespace totals
                    namespace_cpu += sim.actual_cost.cpu_cores;
                    namespace_memory += sim.actual_cost.memory_gb;
                    updated = true;
                }

                // Update namespace utilization metrics
                namespace_util.used_cpu_cores = namespace_cpu;
                namespace_util.used_memory_gb = namespace_memory;
                namespace_util.cpu_percent = (namespace_cpu / allocated_namespace_cpu) * 100.0;
                namespace_util.memory_percent = (namespace_memory / allocated_namespace_memory) * 100.0;
                namespace_util.cluster_total_cpu = total_cluster_cpu;
                namespace_util.cluster_total_memory = total_cluster_memory;
                
                // Update cluster utilization metrics 
                // (total of our namespace plus the base load from other namespaces)
                cluster_util.used_cpu_cores = namespace_cpu + base_cluster_cpu_usage;
                cluster_util.used_memory_gb = namespace_memory + base_cluster_memory_usage;
                cluster_util.cpu_percent = (cluster_util.used_cpu_cores / total_cluster_cpu) * 100.0;
                cluster_util.memory_percent = (cluster_util.used_memory_gb / total_cluster_memory) * 100.0;
                
                // Send utilization updates
                let _ = monitor_state.event_sender.send(AppEvent::ClusterUtilizationUpdated(cluster_util.clone()));
                let _ = monitor_state.event_sender.send(AppEvent::NamespaceUtilizationUpdated(namespace_util.clone()));
                
                tracing::debug!(
                    "Resource update - Namespace: CPU {:.1}% ({:.1}/{:.1}), Memory {:.1}% ({:.1}/{:.1}GB) | Cluster: CPU {:.1}% ({:.1}/{:.1}), Memory {:.1}% ({:.1}/{:.1}GB)", 
                    namespace_util.cpu_percent, namespace_util.used_cpu_cores, namespace_util.allocated_cpu_cores,
                    namespace_util.memory_percent, namespace_util.used_memory_gb, namespace_util.allocated_memory_gb,
                    cluster_util.cpu_percent, cluster_util.used_cpu_cores, cluster_util.total_cpu_cores,
                    cluster_util.memory_percent, cluster_util.used_memory_gb, cluster_util.total_memory_gb
                );
            }

            if updated {
                // Collect current active sims to send full state
                let active_list: Vec<ActiveSimulation> = active_sims.values().cloned().collect();
                // Send update event - ignore error if no receivers yet
                let _ = monitor_state.event_sender.send(AppEvent::ActiveUpdated(active_list));
                tracing::debug!("Sent ActiveUpdated event via broadcast");
            }
        }
    });

    // --- Mock Scheduler Task (Using DB) ---
    let scheduler_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        let max_concurrent_sims = 5;
        let mut mock_completion_timers: HashMap<Uuid, tokio::time::Instant> = HashMap::new();

        loop {
            interval.tick().await;
            tracing::debug!("Scheduler tick");

            let mut queue = scheduler_state.queued_simulations.lock().await;
            let mut active_sims = scheduler_state.active_simulations.lock().await;
            let db_pool = &scheduler_state.db_pool;
            let mut queue_updated = false;
            let mut active_updated = false;

            // 1. Check for completions
            let now = tokio::time::Instant::now();
            let mut completed_ids = Vec::new();
            let mut completed_sim_data = Vec::new(); // Store data for DB update

            for (id, start_time) in &mock_completion_timers {
                if let Some(sim) = active_sims.get(id) {
                    let duration = Duration::from_secs(sim.params.duration_mins as u64 * 5);
                    if now.duration_since(*start_time) >= duration {
                        completed_ids.push(*id);
                    }
                }
            }

            // Remove from active sims and collect data for DB update
            for id in completed_ids {
                if let Some(completed_sim) = active_sims.remove(&id) {
                    mock_completion_timers.remove(&id);
                    tracing::info!(simulation_id = %id, "Mock simulation completed");
                    active_updated = true;
                    
                    // Update last finished simulation
                    let last_finished = LastFinishedSimulation {
                        simulation_id: id,
                        params: completed_sim.params.clone(),
                        predicted_cost: completed_sim.predicted_cost.clone(),
                        actual_cost: completed_sim.actual_cost.clone(),
                        finished_at: chrono::Utc::now(),
                    };
                    
                    // Update last finished simulation in state
                    *scheduler_state.last_finished_simulation.lock().await = Some(last_finished.clone());
                    
                    // Broadcast last finished simulation update
                    let _ = scheduler_state.event_sender.send(AppEvent::LastFinished(last_finished));
                    
                    // Collect data needed for DB insert
                    completed_sim_data.push((completed_sim.params.clone(), completed_sim.actual_cost.clone()));
                }
            }
            // --- Drop active_sims lock before potential DB operations ---
            drop(active_sims);

            // --- Store final costs in DB (using runtime-checked query) ---
            for (params, cost) in completed_sim_data {
                let query_result = sqlx::query(
                    "INSERT OR REPLACE INTO cost_history (chart, node_count, duration_mins, cpu_cores, memory_gb, observed_at) VALUES (?, ?, ?, ?, ?, datetime('now'))"
                )
                .bind(&params.chart)
                .bind(params.node_count)
                .bind(params.duration_mins)
                .bind(cost.cpu_cores)
                .bind(cost.memory_gb)
                .execute(db_pool)
                .await;

                match query_result {
                    Ok(_) => tracing::info!(chart = %params.chart, nodes = %params.node_count, cost = ?cost, "Stored final cost in DB"),
                    Err(e) => tracing::error!(chart = %params.chart, nodes = %params.node_count, "Failed to store cost in DB: {}", e),
                }
            }
            // --------------------------------------------------------

            // 2. Try to schedule new simulations
            while scheduler_state.active_simulations.lock().await.len() < max_concurrent_sims && !queue.is_empty() {
                if let Some(queued_sim) = queue.pop_front() {
                    queue_updated = true;
                    active_updated = true; // Need to broadcast active update too

                    let sim_id = Uuid::new_v4();
                    let active_sim = ActiveSimulation {
                        simulation_id: sim_id,
                        params: queued_sim.params.clone(),
                        predicted_cost: queued_sim.predicted_cost.clone(),
                        actual_cost: ResourceCost::default(),
                    };
                    // Lock active sims again just for insertion
                    scheduler_state.active_simulations.lock().await.insert(sim_id, active_sim.clone());
                    mock_completion_timers.insert(sim_id, tokio::time::Instant::now());

                    tracing::info!(simulation_id = %sim_id, params = ?active_sim.params, "Mock simulation approved and started");
                } else {
                    break;
                }
            }

            // Drop queue lock before broadcast
            drop(queue);

            // Broadcast updates
            if queue_updated {
                 // Lock queue again just for reading the current state
                let current_queue = scheduler_state.queued_simulations.lock().await.iter().cloned().collect();
                let _ = scheduler_state.event_sender.send(AppEvent::QueueUpdated(current_queue));
                 tracing::debug!("Sent QueueUpdated event via broadcast (scheduler)");
            }
            if active_updated {
                 // Lock active sims again just for reading the current state
                 let current_active = scheduler_state.active_simulations.lock().await.values().cloned().collect();
                 let _ = scheduler_state.event_sender.send(AppEvent::ActiveUpdated(current_active));
                 tracing::debug!("Sent ActiveUpdated event via broadcast (scheduler)");
            }
        }
    });

    // Add a task to periodically fetch Kubernetes metrics
    let k8s_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;
            // Try to update from K8s, but fallback to mock data if it fails
            if let Err(e) = update_utilization_from_k8s(k8s_state.clone()).await {
                tracing::warn!("Using mock utilization data: {}", e);
                // We'll continue using the mock data from the monitoring task
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
