Start LARS: Ensure LARS is running with the latest code, including the 30% CPU / 85% Memory admission thresholds.
Submit Mock Simulations: Use curl to send a request to the /mock_submit endpoint. We'll submit enough simulations to likely push the calculated CPU utilization (based on their predicted costs) over the 30% limit.
Check LARS UI/Logs (Optional): You can watch the LARS dashboard to see the mock simulations become active and the cluster utilization climb.
Attempt External Simulation Request: Use curl again to send a request to the /api/v1/request_run endpoint for a new (external) simulation.
Verify Rejection: Check the response from the /api/v1/request_run call. If the mock simulations raised utilization above the threshold, LARS should respond with {"status": "REJECTED", "reason": "..."}. Also, check the LARS logs for messages indicating the rejection and the reason.
Step-by-step commands:
(Assumes LARS is running on http://localhost:9930)