-- Create cost_history table
CREATE TABLE IF NOT EXISTS cost_history (
    chart TEXT NOT NULL,
    node_count INTEGER NOT NULL,
    -- Duration might be useful context, but not part of the primary key for cost
    duration_secs INTEGER NOT NULL,
    -- Observed costs
    cpu_cores REAL NOT NULL,
    memory_gb REAL NOT NULL,
    -- Timestamp of observation
    observed_at TIMESTAMP NOT NULL,

    -- Unique constraint on the simulation parameters IDENTIFYING the cost record
    PRIMARY KEY (chart, node_count, observed_at)
);

-- Optional: Index for faster lookups if needed later
-- CREATE INDEX IF NOT EXISTS idx_cost_history_params ON cost_history (chart, node_count);

-- Index for faster querying by chart and node count (common lookup pattern)
CREATE INDEX IF NOT EXISTS idx_cost_history_chart_nodes ON cost_history (chart, node_count);

-- Optional: You might want an index for querying by time
-- CREATE INDEX IF NOT EXISTS idx_cost_history_time ON cost_history (observed_at);