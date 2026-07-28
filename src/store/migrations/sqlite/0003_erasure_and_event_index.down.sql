DROP TABLE IF EXISTS projection_rebuild_baseline;
DROP TABLE IF EXISTS retention_event_gaps;
DROP TABLE IF EXISTS deletion_tombstones;
DROP TABLE IF EXISTS event_projection_index_state;
DROP INDEX IF EXISTS event_projection_run;
DROP INDEX IF EXISTS event_projection_thread;
DROP INDEX IF EXISTS event_projection_project;
DROP TABLE IF EXISTS event_projection_index;
DELETE FROM projection_state WHERE name = 'domain';
