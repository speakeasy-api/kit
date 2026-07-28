CREATE TABLE deletion_inventory_clock (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    fence INTEGER NOT NULL CHECK (fence > 0)
);
INSERT INTO deletion_inventory_clock (singleton, fence) VALUES (1, 1);

CREATE TABLE deletion_objects (
    object_key TEXT PRIMARY KEY CHECK (object_key <> ''),
    object_kind TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    stored_at_unix_micros INTEGER NOT NULL CHECK (stored_at_unix_micros >= 0),
    archived INTEGER NOT NULL CHECK (archived IN (0, 1)),
    physically_deleted INTEGER NOT NULL DEFAULT 0 CHECK (physically_deleted IN (0, 1)),
    artifact_digest TEXT,
    policy_json BLOB NOT NULL
);

CREATE TABLE deletion_legal_holds (
    hold_id TEXT PRIMARY KEY,
    scope_kind TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    placed_at_unix_micros INTEGER NOT NULL,
    released_at_unix_micros INTEGER
);

CREATE TABLE deletion_artifact_references (
    reference_id TEXT PRIMARY KEY,
    artifact_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    expires_at_unix_micros INTEGER
);

CREATE TABLE deletion_backup_generations (
    generation_id TEXT PRIMARY KEY,
    created_at_unix_micros INTEGER NOT NULL,
    expires_at_unix_micros INTEGER
);

CREATE TABLE deletion_backup_contents (
    generation_id TEXT NOT NULL REFERENCES deletion_backup_generations(generation_id) ON DELETE CASCADE,
    object_key TEXT NOT NULL,
    PRIMARY KEY (generation_id, object_key)
);

CREATE TABLE deletion_jobs (
    job_id TEXT PRIMARY KEY,
    principal_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    object_key TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    resource_version INTEGER NOT NULL CHECK (resource_version > 0),
    policy_snapshot_json BLOB NOT NULL,
    policy_json BLOB NOT NULL,
    earliest_physical_unix_micros INTEGER,
    state TEXT NOT NULL CHECK (state IN (
        'requested', 'evaluating', 'waiting_for_policy', 'physically_deleting',
        'completed', 'blocked', 'failed'
    )),
    version INTEGER NOT NULL CHECK (version > 0),
    fence INTEGER NOT NULL CHECK (fence > 0),
    blockers_json BLOB NOT NULL,
    requested_at_unix_micros INTEGER NOT NULL,
    completed_at_unix_micros INTEGER,
    failure TEXT,
    worker_id TEXT,
    lease_until_unix_micros INTEGER,
    effect_unknown INTEGER NOT NULL DEFAULT 0 CHECK (effect_unknown IN (0, 1)),
    UNIQUE (principal_id, object_key, idempotency_key)
);

CREATE INDEX deletion_jobs_ready
ON deletion_jobs (state, earliest_physical_unix_micros, requested_at_unix_micros);

CREATE TABLE deletion_job_audit (
    job_id TEXT NOT NULL REFERENCES deletion_jobs(job_id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    state TEXT NOT NULL,
    at_unix_micros INTEGER NOT NULL,
    PRIMARY KEY (job_id, sequence)
);

CREATE TRIGGER deletion_object_authority_update
AFTER UPDATE OF principal_id, project_id, stored_at_unix_micros, artifact_digest, policy_json, physically_deleted
ON deletion_objects
WHEN OLD.principal_id <> NEW.principal_id
  OR OLD.project_id <> NEW.project_id
  OR OLD.stored_at_unix_micros <> NEW.stored_at_unix_micros
  OR OLD.artifact_digest IS NOT NEW.artifact_digest
  OR OLD.policy_json <> NEW.policy_json
  OR OLD.physically_deleted <> NEW.physically_deleted
BEGIN
    UPDATE deletion_inventory_clock SET fence = fence + 1 WHERE singleton = 1;
END;

CREATE TRIGGER deletion_hold_insert AFTER INSERT ON deletion_legal_holds BEGIN
    UPDATE deletion_inventory_clock SET fence = fence + 1 WHERE singleton = 1;
END;
CREATE TRIGGER deletion_hold_update AFTER UPDATE ON deletion_legal_holds BEGIN
    UPDATE deletion_inventory_clock SET fence = fence + 1 WHERE singleton = 1;
END;
CREATE TRIGGER deletion_hold_delete AFTER DELETE ON deletion_legal_holds BEGIN
    UPDATE deletion_inventory_clock SET fence = fence + 1 WHERE singleton = 1;
END;
CREATE TRIGGER deletion_reference_insert AFTER INSERT ON deletion_artifact_references BEGIN
    UPDATE deletion_inventory_clock SET fence = fence + 1 WHERE singleton = 1;
END;
CREATE TRIGGER deletion_reference_update AFTER UPDATE ON deletion_artifact_references BEGIN
    UPDATE deletion_inventory_clock SET fence = fence + 1 WHERE singleton = 1;
END;
CREATE TRIGGER deletion_reference_delete AFTER DELETE ON deletion_artifact_references BEGIN
    UPDATE deletion_inventory_clock SET fence = fence + 1 WHERE singleton = 1;
END;
CREATE TRIGGER deletion_backup_insert AFTER INSERT ON deletion_backup_generations BEGIN
    UPDATE deletion_inventory_clock SET fence = fence + 1 WHERE singleton = 1;
END;
CREATE TRIGGER deletion_backup_update AFTER UPDATE ON deletion_backup_generations BEGIN
    UPDATE deletion_inventory_clock SET fence = fence + 1 WHERE singleton = 1;
END;
CREATE TRIGGER deletion_backup_delete AFTER DELETE ON deletion_backup_generations BEGIN
    UPDATE deletion_inventory_clock SET fence = fence + 1 WHERE singleton = 1;
END;
CREATE TRIGGER deletion_backup_content_insert AFTER INSERT ON deletion_backup_contents BEGIN
    UPDATE deletion_inventory_clock SET fence = fence + 1 WHERE singleton = 1;
END;
CREATE TRIGGER deletion_backup_content_delete AFTER DELETE ON deletion_backup_contents BEGIN
    UPDATE deletion_inventory_clock SET fence = fence + 1 WHERE singleton = 1;
END;
