# LARS - Design Document

This document outlines the proposed high-level design for LARS (Lab Automated Resource Scheduler).

## 1. Architecture Overview

LARS will be implemented as a standalone, long-running Rust application, likely deployed as a Kubernetes Deployment within the lab cluster.

It will consist of several key components:

1.  **API Server:** Handles incoming HTTP requests (`/request_run`, `/simulation_complete`) from simulation orchestrators like `run.py`. (Likely using a Rust web framework like `axum` or `actix-web`).
2.  **Scheduler Core:** Contains the main logic for evaluating simulation requests based on predicted costs and current load.
3.  **Monitoring Module:** Responsible for periodically fetching resource metrics from:
    *   Kubernetes API Server (for node CPU/Memory usage and pod metrics). (Using `kube-rs` crate).
    *   Prometheus/VictoriaMetrics (for network bandwidth and potentially other metrics). (Using a Prometheus client crate).
4.  **Cost Database:** Stores and retrieves historical simulation parameters and their observed resource costs. (Potentially using `SQLite` via `rusqlite` for embedded persistence, or connecting to an external DB).
5.  **State Manager:** Tracks the set of currently approved/running simulations and their predicted resource footprints. (Likely in-memory, potentially backed by the database for recovery).

```mermaid
graph TD
    subgraph LARS Service (Rust Application)
        A[API Server (axum/actix-web)] --> B(Scheduler Core);
        B --> C{Monitoring Module};
        B --> D[Cost Database (SQLite/rusqlite)];
        B --> E[State Manager];
        C --> F[Kubernetes API (kube-rs)];
        C --> G[Prometheus API (prometheus-client)];
        D -- Stores/Retrieves --> H{Persistent Storage};
        E -- Tracks --> B;
    end

    I(Simulation Orchestrator e.g. run.py) -- HTTP POST /request_run --> A;
    I -- HTTP POST /simulation_complete --> A;
    A -- HTTP Response (approved/denied) --> I;

    F -- Node/Pod Metrics --> K(Kubernetes Cluster);
    G -- Bandwidth/Other Metrics --> L(Prometheus/VictoriaMetrics);

    K -- Metrics --> C;
    L -- Metrics --> C;

```

## 2. Data Flow: Admission Request (`/request_run`)

1.  `run.py` sends `POST /request_run` with simulation params `{sim_params}`.
2.  API Server receives request, passes `{sim_params}` to Scheduler Core.
3.  Scheduler Core queries Cost Database for historical costs matching `{sim_params}`.
4.  Scheduler Core requests current overall cluster usage from Monitoring Module.
5.  Scheduler Core queries State Manager for predicted costs of currently active simulations.
6.  Scheduler Core calculates `predicted_cost = predict(historical_cost or default)`.
7.  Scheduler Core calculates `future_load = current_usage + sum(active_sim_costs) + predicted_cost`.
8.  Scheduler Core compares `future_load` against configured `target_utilization`.
9.  If `future_load <= target_utilization`:
    *   Generate unique `simulation_id`.
    *   Update State Manager: add `simulation_id` with `predicted_cost`.
    *   API Server responds `{"decision": "approved", "simulation_id": simulation_id}`.
10. Else:
    *   API Server responds `{"decision": "denied"}`.

## 3. Data Flow: Learning & Monitoring

1.  Monitoring Module periodically fetches overall cluster metrics (Node CPU/Mem, Network Bandwidth) and stores the latest values.
2.  Monitoring Module identifies pods/resources associated with active `simulation_id`s (via K8s labels set during Helm deployment).
3.  Monitoring Module tracks resource usage specifically for these labeled resources.
4.  When `POST /simulation_complete` is received for a `simulation_id`:
    *   State Manager marks the simulation as completed.
    *   Scheduler Core retrieves the monitored peak resource usage for that `simulation_id` from the Monitoring Module.
    *   Scheduler Core calculates the differential cost (`peak_usage - baseline_before_start`).
    *   Scheduler Core stores the original `{sim_params}` and the calculated `differential_cost` into the Cost Database.
    *   State Manager removes the `simulation_id` from the active list.

## 4. Cost Model & Prediction (Initial)

*   **Database Schema:** A simple table mapping simulation parameters (e.g., `chart`, `nodecount`, `publisher_enabled`, etc.) to observed costs (`peak_cpu_delta`, `peak_mem_delta`, `peak_network_delta`). Could use JSON blobs for parameters initially for flexibility.
*   **Lookup:** Find exact matches for parameters first. If no exact match, find the "closest" match (e.g., similar node count for the same chart) or use a pre-defined default cost for that chart type.
*   **Prediction:** Initially, the predicted cost will simply be the historical cost found in the database or the default. Future enhancements could involve averaging recent runs or simple linear scaling based on parameters like node count.

## 5. Persistence

*   The Cost Database will be the primary persistent store. Using embedded SQLite simplifies deployment, storing the DB file in a PersistentVolume within Kubernetes.

## 6. Configuration

*   Configuration (API ports, K8s/Prometheus endpoints, DB path, utilization targets) will be managed via environment variables or a configuration file, following standard Rust practices.

## 7. Error Handling

*   API calls should handle missing parameters gracefully.
*   Failures to connect to K8s or Prometheus should be logged, and potentially lead to the scheduler entering a "safe mode" where it denies all requests until connectivity is restored.
*   Database errors should be logged.

## 8. Web User Interface

To provide visibility into LARS's state and activity, a web interface will be included.

*   **Technology:**
    *   Backend Framework: `Axum` (Rust)
    *   Templating Engine: `MiniJinja` (Rust)
    *   CSS Framework: `TailwindCSS`
*   **Real-time Updates:** Server-Sent Events (SSE) will be used to push updates from the server to connected web clients without requiring polling. An endpoint like `GET /status-stream` will stream updates.
*   **Functionality:** The UI will display:
    *   Overall lab status (if tracked by LARS).
    *   A list of simulations currently **queued** (waiting for approval), showing their parameters and predicted resource costs.
    *   A list of simulations currently **active** (running), showing their parameters, predicted costs, and updating **actual** resource usage (avg/peak/median) as monitored by LARS.
*   **Backend Changes:**
    *   The `State Manager` must be updated to track queued simulation requests in addition to active ones.
    *   The `Monitoring Module` needs to periodically update the shared state with the latest *actual* resource usage for active simulations.
    *   New Axum routes are required:
        *   `GET /`: Serves the main HTML page rendered by MiniJinja.
        *   `GET /static/*`: Serves static assets (Tailwind CSS output, potentially JS).
        *   `GET /status-stream`: Handles SSE connections and pushes state updates.
*   **Frontend:**
    *   HTML templates (`.html.j2`) rendered by MiniJinja.
    *   Styling using TailwindCSS utility classes.
    *   Minimal vanilla JavaScript to connect to the SSE endpoint and update the DOM based on received event data (e.g., adding/removing simulations, updating usage figures).
*   **Development Workflow:**
    *   `tailwindcss` CLI run in watch mode (`--watch`) to automatically rebuild CSS on changes to templates.
    *   `cargo-watch` to monitor Rust code changes and restart the Axum server.
    *   `tower-livereload` (or similar) integrated into Axum (in debug builds) to trigger browser reloads upon server restart.