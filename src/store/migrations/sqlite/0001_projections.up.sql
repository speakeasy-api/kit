CREATE TABLE projection_state (
    name TEXT PRIMARY KEY CHECK (name <> ''),
    canonical_bytes BLOB NOT NULL,
    digest BLOB NOT NULL CHECK (length(digest) = 32),
    checkpoint INTEGER NOT NULL CHECK (checkpoint >= 0),
    updated_at TEXT NOT NULL
);

CREATE TABLE store_clock (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    unix_micros INTEGER NOT NULL CHECK (unix_micros >= 0)
);

INSERT INTO store_clock (singleton, unix_micros) VALUES (1, 0);

CREATE TRIGGER projection_checkpoint_insert
BEFORE INSERT ON projection_state
WHEN NEW.checkpoint > COALESCE((SELECT position FROM commit_watermark WHERE singleton = 1), -1)
BEGIN
    SELECT RAISE(ABORT, 'projection checkpoint exceeds committed prefix');
END;

CREATE TRIGGER projection_checkpoint_update
BEFORE UPDATE OF checkpoint ON projection_state
WHEN NEW.checkpoint > COALESCE((SELECT position FROM commit_watermark WHERE singleton = 1), -1)
BEGIN
    SELECT RAISE(ABORT, 'projection checkpoint exceeds committed prefix');
END;
