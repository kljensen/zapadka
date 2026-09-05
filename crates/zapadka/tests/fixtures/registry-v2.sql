-- Frozen registry DDL from v0.5.4; never generated from the implementation under test.
-- One row per nontransactional statement that was started and has not been
-- resolved. Normally empty.
CREATE TABLE zapadka.nontransactional_attempts (
    migration_id      uuid        PRIMARY KEY,
    slug              text        NOT NULL,
    definition_sha256 text        NOT NULL,
    deploy_sha256     text        NOT NULL,
    depends           uuid[]      NOT NULL,
    started_at        timestamptz NOT NULL DEFAULT now(),
    run_id            uuid        NOT NULL,
    session_user_name text        NOT NULL,
    server_version    text        NOT NULL,
    zapadka_version   text        NOT NULL
);

COMMENT ON TABLE zapadka.nontransactional_attempts IS
    'Nontransactional statements started but not resolved. A row here blocks '
    'deployment: only a person can say whether the statement took effect. '
    'Resolve it with `zapadka resolve`, never by deleting the row.';
