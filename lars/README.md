# LARS - Lab Automated Resource Scheduler

LARS (Lab Automated Resource Scheduler) is a service designed to intelligently manage and schedule simulation workloads within the lab environment.

## Purpose

The primary goal of LARS is to maximize the utilization of lab computing resources (CPU, Memory, Network Bandwidth) by safely running multiple simulations concurrently, while preventing resource exhaustion that could destabilize the lab or interfere with simulation results.

It acts as a gatekeeper, learning the resource cost of different simulation configurations and using this knowledge to predict the impact of new requests. It only approves new simulations if the predicted total load remains within acceptable operational limits.

## Technology

LARS is written in **Rust** for its performance, reliability, and memory safety characteristics, which are crucial for a long-running infrastructure service.

## Interaction

LARS provides an API for simulation orchestrators (like the `dst-prefect-workflows/prefect/run.py` script) to request permission before deploying a simulation. `run.py` queries LARS with the simulation parameters, and LARS responds whether the simulation can proceed immediately or should be deferred. 