CREATE TABLE event_projection_index (
    commit_position INTEGER PRIMARY KEY REFERENCES events(commit_position),
    project_id TEXT NOT NULL,
    thread_id TEXT,
    run_id TEXT,
    event_class TEXT NOT NULL CHECK (event_class IN ('event', 'terminal', 'experiment')),
    stored_at_unix_micros INTEGER NOT NULL CHECK (stored_at_unix_micros >= 0),
    erased INTEGER NOT NULL DEFAULT 0 CHECK (erased IN (0, 1))
);
CREATE INDEX event_projection_project
ON event_projection_index (project_id, erased, commit_position);
CREATE INDEX event_projection_thread
ON event_projection_index (thread_id, erased, commit_position);
CREATE INDEX event_projection_run
ON event_projection_index (run_id, erased, commit_position);

CREATE TABLE event_projection_index_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    indexed_through INTEGER NOT NULL CHECK (indexed_through >= 0)
);
INSERT INTO event_projection_index_state (singleton, indexed_through) VALUES (1, 0);

CREATE TABLE deletion_tombstones (
    target_sha256 BLOB PRIMARY KEY CHECK (length(target_sha256) = 32),
    object_kind TEXT NOT NULL,
    completed_at_unix_micros INTEGER NOT NULL,
    erased_event_count INTEGER NOT NULL CHECK (erased_event_count >= 0),
    outcome TEXT NOT NULL CHECK (outcome = 'erased')
);

CREATE TABLE retention_event_gaps (
    project_id TEXT NOT NULL,
    event_class TEXT NOT NULL,
    first_available_position INTEGER NOT NULL CHECK (first_available_position > 0),
    compacted_through INTEGER NOT NULL CHECK (compacted_through > 0),
    cursor_expired_at_unix_micros INTEGER NOT NULL,
    cursor_expiry_snapshot BLOB NOT NULL,
    PRIMARY KEY (project_id, event_class)
);

CREATE TABLE projection_rebuild_baseline (
    name TEXT PRIMARY KEY,
    canonical_bytes BLOB NOT NULL,
    digest BLOB NOT NULL CHECK (length(digest) = 32),
    checkpoint INTEGER NOT NULL CHECK (checkpoint >= 0)
);

-- Version 3 changes the domain projection's canonical format. Retained events are
-- the migration source of truth; dropping only this derived row forces a rebuild.
DELETE FROM projection_state WHERE name = 'domain';
