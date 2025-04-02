-- Create cost_history table
CREATE TABLE IF NOT EXISTS cost_history (
    chart TEXT NOT NULL,
    node_count INTEGER NOT NULL,
    -- Duration might be useful context, but not part of the primary key for cost
    duration_mins INTEGER NOT NULL,
    -- Observed costs
    cpu_cores REAL NOT NULL,
    memory_gb REAL NOT NULL,
    -- Timestamp of observation
    observed_at DATETIME DEFAULT CURRENT_TIMESTAMP,

    -- Unique constraint on the simulation parameters IDENTIFYING the cost record
    PRIMARY KEY (chart, node_count)
);

-- Optional: Index for faster lookups if needed later
-- CREATE INDEX IF NOT EXISTS idx_cost_history_params ON cost_history (chart, node_count);