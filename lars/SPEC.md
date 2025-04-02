# LARS - Specification

This document outlines the functional and non-functional requirements for LARS (Lab Automated Resource Scheduler).

## 1. Functional Requirements

### 1.1. Admission Control API
    - LARS MUST expose an HTTP API endpoint (e.g., `POST /request_run`).
    - This endpoint MUST accept a payload describing the parameters of a requested simulation (e.g., chart type, node count, publisher settings, requested duration).
    - The endpoint MUST evaluate the request based on current lab load and predicted cost.
    - The endpoint MUST respond with a decision (`approved` or `denied`).
    - If approved, the response MUST include a unique `simulation_id` for tracking.

### 1.2. Simulation Completion API
    - LARS MUST expose an HTTP API endpoint (e.g., `POST /simulation_complete`).
    - This endpoint MUST accept a payload containing the `simulation_id` of a completed simulation.
    - Upon receiving this notification, LARS MUST mark the simulation as complete internally.

### 1.3. Resource Monitoring
    - LARS MUST continuously monitor the overall resource usage of the Kubernetes cluster nodes (CPU, Memory).
    - LARS MUST continuously monitor the overall network bandwidth usage (via Prometheus/VictoriaMetrics).
    - LARS MUST be able to identify and monitor the specific resource usage attributed to individual simulations (likely via Kubernetes labels/selectors associated with the `simulation_id`).

### 1.4. Cost Modeling & Prediction
    - LARS MUST maintain a persistent database storing historical resource costs (e.g., peak CPU delta, peak Memory delta, peak Network delta) associated with completed simulations, keyed by simulation parameters.
    - When evaluating a `/request_run`, LARS MUST query this database to find the cost of similar past simulations.
    - LARS MUST use historical data (or heuristics/defaults if no history exists) to predict the resource impact of the requested simulation.
    - LARS MUST factor in the predicted costs of already approved, currently running simulations when making a decision.

### 1.5. Scheduling Logic
    - LARS MUST maintain a configurable target utilization threshold (e.g., 80% CPU, 80% Memory, 75% Bandwidth).
    - LARS MUST approve a `/request_run` only if `current_usage + predicted_cost_of_request + predicted_cost_of_other_approved_sims <= target_utilization`.
    - LARS MUST update its internal state when a simulation is approved to account for its predicted resource allocation.

### 1.6. Cost Learning
    - After a simulation completes (signaled by `/simulation_complete`), LARS MUST analyze the monitored resource usage specifically attributed to that simulation's `simulation_id`.
    - LARS MUST calculate the actual differential resource cost (peak usage during simulation - baseline before simulation).
    - LARS MUST update its persistent database with this newly observed cost data, refining its future predictions.

## 2. Non-Functional Requirements

### 2.1. Reliability
    - LARS MUST be designed for continuous, long-term operation.
    - Failures in LARS should prevent new simulations from starting but MUST NOT interfere with simulations already running.
    - LARS MUST handle transient failures (e.g., temporary inability to reach Kubernetes API or Prometheus) gracefully, potentially by temporarily denying requests until connectivity is restored.

### 2.2. Performance
    - API responses (`/request_run`, `/simulation_complete`) SHOULD be returned quickly (e.g., within milliseconds to a few seconds) to avoid blocking the simulation orchestrator.
    - Resource monitoring SHOULD be efficient and not impose significant overhead on the cluster or monitoring systems.

### 2.3. Persistence
    - Learned cost data MUST survive restarts of the LARS service.

### 2.4. Configurability
    - Target utilization thresholds MUST be configurable.
    - API endpoints for Kubernetes and Prometheus MUST be configurable.
    - Database connection/path MUST be configurable.

### 2.5. Observability
    - LARS SHOULD expose metrics about its own operation (e.g., requests processed, decisions made, current predicted load, database size).
    - LARS SHOULD produce structured logs for debugging and monitoring.

### 2.6. Testability
    - Core logic components (scheduler, cost prediction, state management) MUST be designed in a way that facilitates unit testing.
    - Unit tests SHOULD be provided for critical logic paths, covering expected behavior, edge cases, and error handling where applicable.
    - Mocks or test doubles SHOULD be used where necessary to isolate components from external dependencies (like Kubernetes API, Prometheus, Database) during unit tests. 