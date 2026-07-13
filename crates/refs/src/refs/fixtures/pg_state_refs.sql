CREATE TABLE refs (
    repo_id UUID NOT NULL,
    name TEXT NOT NULL,
    is_thread BOOLEAN NOT NULL,
    state_id BYTEA NOT NULL CHECK (octet_length(state_id) = 32),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (repo_id, name)
);

CREATE TABLE heads (
    repo_id UUID PRIMARY KEY,
    thread TEXT,
    state_id BYTEA CHECK (state_id IS NULL OR octet_length(state_id) = 32),
    CHECK ((thread IS NULL) <> (state_id IS NULL))
);
