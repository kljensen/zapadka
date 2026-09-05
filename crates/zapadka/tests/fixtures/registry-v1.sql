-- Frozen registry DDL from v0.5.4; never generated from the implementation under test.
CREATE SCHEMA IF NOT EXISTS zapadka;

COMMENT ON SCHEMA zapadka IS
    'Zapadka migration registry. Managed by the zapadka tool; do not edit by hand.';

-- Exactly one row: this database belongs to exactly one Zapadka project.
CREATE TABLE zapadka.meta (
    singleton               boolean     PRIMARY KEY DEFAULT true
                                        CONSTRAINT meta_is_singleton CHECK (singleton),
    project_id              uuid        NOT NULL,
    registry_format_version integer     NOT NULL,
    created_at              timestamptz NOT NULL DEFAULT now(),
    created_by              text        NOT NULL
);

-- Current state: the immutable facts about every applied migration.
CREATE TABLE zapadka.applied_migrations (
    migration_id      uuid        PRIMARY KEY,
    slug              text        NOT NULL,
    definition_sha256 text        NOT NULL,
    deploy_sha256     text        NOT NULL,
    depends           uuid[]      NOT NULL,
    transaction_mode  text        NOT NULL
                                  CHECK (transaction_mode IN ('required', 'forbidden')),
    applied_at        timestamptz NOT NULL DEFAULT now(),
    run_id            uuid        NOT NULL
);

-- Append-only history.
CREATE TABLE zapadka.events (
    run_id            uuid        NOT NULL,
    sequence          integer     NOT NULL,
    recorded_at       timestamptz NOT NULL DEFAULT now(),
    migration_id      uuid,
    action            text        NOT NULL,
    outcome           text        NOT NULL,
    transaction_mode  text,
    definition_sha256 text,
    script_role       text,
    script_sha256     text,
    duration_ms       bigint,
    sqlstate          text,
    message           text,
    detail            text,
    session_user_name text        NOT NULL,
    current_user_name text        NOT NULL,
    server_version    text        NOT NULL,
    zapadka_version   text        NOT NULL,
    PRIMARY KEY (run_id, sequence)
);

CREATE INDEX events_recorded_at_idx ON zapadka.events (recorded_at DESC);
CREATE INDEX events_migration_idx ON zapadka.events (migration_id, recorded_at DESC);

-- History is evidence. Making it append-only in the database means a Zapadka
-- bug cannot quietly rewrite the record of what it did.
CREATE FUNCTION zapadka.events_are_append_only() RETURNS trigger
    LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'zapadka.events is append-only; % is not permitted', TG_OP
        USING ERRCODE = 'restrict_violation';
END;
$$;

CREATE TRIGGER events_append_only
    BEFORE UPDATE OR DELETE ON zapadka.events
    FOR EACH ROW EXECUTE FUNCTION zapadka.events_are_append_only();

-- TRUNCATE is not an UPDATE or a DELETE, and the table's owner -- which is
-- normally the deploying role -- can issue it. Without this, the whole history
-- could be erased by a statement the row-level trigger never sees.
CREATE TRIGGER events_no_truncate
    BEFORE TRUNCATE ON zapadka.events
    FOR EACH STATEMENT EXECUTE FUNCTION zapadka.events_are_append_only();
