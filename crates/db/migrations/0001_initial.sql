CREATE TABLE IF NOT EXISTS base_nodes (
    gen       INTEGER NOT NULL,
    path      TEXT NOT NULL,
    parent    TEXT NOT NULL,
    kind      INTEGER NOT NULL,
    oid       TEXT,
    mode      INTEGER NOT NULL,
    size      INTEGER,
    PRIMARY KEY (gen, path)
);

CREATE INDEX IF NOT EXISTS idx_base_parent ON base_nodes(gen, parent);

CREATE TABLE IF NOT EXISTS overlay_nodes (
    path       TEXT PRIMARY KEY,
    kind       TEXT NOT NULL,
    backing    TEXT,
    mode       INTEGER NOT NULL,
    size       INTEGER NOT NULL DEFAULT 0,
    mtime_ns   INTEGER NOT NULL,
    source_oid TEXT
);

CREATE TABLE IF NOT EXISTS pack_index (
    oid       TEXT PRIMARY KEY,
    pack_id   TEXT NOT NULL,
    offset    INTEGER NOT NULL,
    comp_size INTEGER NOT NULL,
    raw_size  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sync_state (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
