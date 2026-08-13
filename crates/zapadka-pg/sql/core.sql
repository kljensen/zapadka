-- Zapadka's SQL assertion library: runner state and scalar assertions.
--
-- Installed into the reserved test schema. Every assertion records a typed row
-- and returns a boolean; nothing here builds, emits, or parses TAP. See
-- docs/adr/0004-separate-deployment-verification-from-database-tests.md.
--
-- The public API deliberately matches pgTAP's names and argument types. It is a
-- good API that people already know, and matching it means a test file is not a
-- dialect anyone has to learn twice.

-- The shape of the capture tables. Bumped when a column changes meaning, so a
-- binary meeting an older installed library refuses rather than misreading it.
CREATE OR REPLACE FUNCTION _protocol_version() RETURNS integer AS $$
    SELECT 1;
$$ LANGUAGE sql IMMUTABLE;

-- The installed library's version, for a person inspecting a database.
--
-- Deliberately not `pgtap_version()`: this is not pgTAP, and answering to that
-- name would tell a test file it could rely on pgTAP behaviour Zapadka does not
-- implement.
CREATE OR REPLACE FUNCTION zapadka_test_version() RETURNS text AS $$
    SELECT '2';
$$ LANGUAGE sql IMMUTABLE;

-- Creates the per-file capture tables.
--
-- Called by the runner inside the transaction it owns, so `ON COMMIT DROP`
-- disposes of everything: there is no run id to thread, nothing to clean up,
-- and no way for one file's results to be visible to another.
CREATE OR REPLACE FUNCTION _begin_run() RETURNS void AS $$
DECLARE
    leftover text;
BEGIN
    -- Dropped first so a second run in one session -- someone stepping through
    -- files by hand in psql -- starts clean instead of failing on a name.
    -- Checked rather than `IF EXISTS`, which emits a NOTICE for every absent
    -- table on the very first run.
    FOR leftover IN
        SELECT unnest(ARRAY['__zapadka_assertion', '__zapadka_note', '__zapadka_run'])
    LOOP
        IF to_regclass('pg_temp.' || quote_ident(leftover)) IS NOT NULL THEN
            EXECUTE format('DROP TABLE pg_temp.%I', leftover);
        END IF;
    END LOOP;

    CREATE TEMP TABLE __zapadka_run (
        singleton        boolean PRIMARY KEY DEFAULT true CHECK (singleton),
        protocol_version integer NOT NULL,
        plan_mode        text CHECK (plan_mode IN ('count', 'no_plan')),
        declared_plan    integer CHECK (declared_plan >= 0),
        skip_all_reason  text,
        finished         boolean NOT NULL DEFAULT false,
        todo_reason      text,
        todo_remaining   integer,
        -- A plan is a claim about how many assertions will run; it only makes
        -- sense alongside a count.
        CHECK ((plan_mode = 'count') = (declared_plan IS NOT NULL))
    ) ON COMMIT DROP;

    CREATE TEMP TABLE __zapadka_assertion (
        number           integer PRIMARY KEY CHECK (number > 0),
        kind             text NOT NULL,
        passed           boolean NOT NULL,
        description      text,
        directive        text CHECK (directive IN ('todo', 'skip')),
        directive_reason text,
        detail           jsonb,
        recorded_at      timestamptz NOT NULL DEFAULT clock_timestamp(),
        -- A reason without a directive is meaningless.
        CHECK (directive IS NOT NULL OR directive_reason IS NULL)
    ) ON COMMIT DROP;

    -- Free-standing notes. `after_assertion` is nullable on purpose: a note
    -- written before the first assertion belongs to no assertion, and pretending
    -- otherwise would attach it to something arbitrary.
    CREATE TEMP TABLE __zapadka_note (
        ordinal         integer PRIMARY KEY,
        after_assertion integer,
        message         text NOT NULL
    ) ON COMMIT DROP;

    INSERT INTO __zapadka_run (protocol_version) VALUES (_protocol_version());
END;
$$ LANGUAGE plpgsql;

-- Fails with a clear message when a test file runs outside the runner.
CREATE OR REPLACE FUNCTION _require_run() RETURNS void AS $$
BEGIN
    IF to_regclass('pg_temp.__zapadka_run') IS NULL THEN
        RAISE EXCEPTION 'no test run is active'
            USING HINT = 'run this file with `zapadka test`, or call '
                      || '_begin_run() first when stepping through it by hand';
    END IF;
END;
$$ LANGUAGE plpgsql;

-- The next assertion number.
CREATE OR REPLACE FUNCTION _next_number() RETURNS integer AS $$
DECLARE
    candidate integer;
BEGIN
    SELECT coalesce(max(number), 0) + 1 INTO candidate FROM pg_temp.__zapadka_assertion;
    RETURN candidate;
END;
$$ LANGUAGE plpgsql;

-- Records one assertion and returns whether it actually passed.
--
-- Returns the *actual* outcome, not the effective one: `SELECT is(1, 2, 'x')`
-- in psql should say `f` even under a TODO. Whether a failure fails the run is
-- the runner's decision, made from the directive.
CREATE OR REPLACE FUNCTION _record(
    kind        text,
    passed      boolean,
    description text DEFAULT NULL,
    detail      jsonb DEFAULT NULL
) RETURNS boolean AS $$
DECLARE
    todo_why  text;
    todo_left integer;
    directive text;
    reason    text;
    number    integer;
BEGIN
    PERFORM _require_run();
    SELECT todo_reason, todo_remaining INTO todo_why, todo_left
      FROM pg_temp.__zapadka_run;

    IF todo_why IS NOT NULL THEN
        directive := 'todo';
        reason    := todo_why;
        -- `todo(why, how_many)` covers a fixed number of assertions.
        IF todo_left IS NOT NULL THEN
            IF todo_left <= 1 THEN
                UPDATE pg_temp.__zapadka_run
                   SET todo_reason = NULL, todo_remaining = NULL;
            ELSE
                UPDATE pg_temp.__zapadka_run
                   SET todo_remaining = todo_left - 1;
            END IF;
        END IF;
    END IF;

    number := _next_number();
    INSERT INTO pg_temp.__zapadka_assertion
        (number, kind, passed, description, directive, directive_reason, detail)
    VALUES
        (number, kind, coalesce(passed, false), description, directive, reason, detail);

    RETURN coalesce(passed, false);
END;
$$ LANGUAGE plpgsql;

-- Describes a value for a diagnostic: its JSON form, its text form, and its
-- type.
--
-- Both forms are kept deliberately. JSON is for machines and for structured
-- values; PostgreSQL's own text output is often better for a person and is the
-- only sensible representation for types with no JSON structure. Neither is
-- promised to reconstruct the original datum.
CREATE OR REPLACE FUNCTION _describe(value anyelement) RETURNS jsonb AS $$
    SELECT jsonb_build_object(
        'json',    coalesce(to_jsonb($1), 'null'::jsonb),
        'display', $1::text,
        'type',    pg_typeof($1)::text,
        'is_null', $1 IS NULL
    );
$$ LANGUAGE sql;

-- -- Plan and completion ----------------------------------------------------
--
-- Both are optional. `1..N` exists so a *text* consumer can notice a truncated
-- stream; a runner reading a table already knows whether the file finished.
-- They are kept because declaring an expected count is still a real check, and
-- because pgTAP files carry them.

CREATE OR REPLACE FUNCTION plan(integer) RETURNS boolean AS $$
BEGIN
    PERFORM _require_run();
    IF EXISTS (SELECT 1 FROM pg_temp.__zapadka_run WHERE plan_mode IS NOT NULL) THEN
        RAISE EXCEPTION 'the plan has already been declared';
    END IF;
    UPDATE pg_temp.__zapadka_run SET plan_mode = 'count', declared_plan = $1;
    RETURN true;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION no_plan() RETURNS boolean AS $$
BEGIN
    PERFORM _require_run();
    IF EXISTS (SELECT 1 FROM pg_temp.__zapadka_run WHERE plan_mode IS NOT NULL) THEN
        RAISE EXCEPTION 'the plan has already been declared';
    END IF;
    UPDATE pg_temp.__zapadka_run SET plan_mode = 'no_plan';
    RETURN true;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION finish() RETURNS boolean AS $$
BEGIN
    PERFORM _require_run();
    UPDATE pg_temp.__zapadka_run SET finished = true;
    RETURN true;
END;
$$ LANGUAGE plpgsql;

-- -- Notes -----------------------------------------------------------------

CREATE OR REPLACE FUNCTION diag(text) RETURNS boolean AS $$
DECLARE
    -- Named rather than referenced as $1 inside the INSERT, because a parameter
    -- called `message` would be ambiguous against the column of that name.
    body      text := $1;
    last_seen integer;
    next_ord  integer;
BEGIN
    PERFORM _require_run();
    SELECT max(number) INTO last_seen FROM pg_temp.__zapadka_assertion;
    SELECT coalesce(max(ordinal), 0) + 1 INTO next_ord FROM pg_temp.__zapadka_note;
    INSERT INTO pg_temp.__zapadka_note (ordinal, after_assertion, message)
    VALUES (next_ord, last_seen, body);
    RETURN true;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION note(text) RETURNS boolean AS $$
    SELECT diag($1);
$$ LANGUAGE sql;

-- -- Directives ------------------------------------------------------------

CREATE OR REPLACE FUNCTION todo_start(why text DEFAULT NULL) RETURNS boolean AS $$
BEGIN
    PERFORM _require_run();
    UPDATE pg_temp.__zapadka_run
       SET todo_reason = coalesce(why, ''), todo_remaining = NULL;
    RETURN true;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION todo_end() RETURNS boolean AS $$
BEGIN
    PERFORM _require_run();
    UPDATE pg_temp.__zapadka_run SET todo_reason = NULL, todo_remaining = NULL;
    RETURN true;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION todo(why text, how_many integer) RETURNS boolean AS $$
BEGIN
    PERFORM _require_run();
    UPDATE pg_temp.__zapadka_run
       SET todo_reason = coalesce(why, ''), todo_remaining = how_many;
    RETURN true;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION todo(how_many integer, why text) RETURNS boolean AS $$
    SELECT todo($2, $1);
$$ LANGUAGE sql;

-- Records `how_many` assertions as skipped.
--
-- In pgTAP a skip is a pass with `# SKIP` glued onto the text, which is why the
-- outcome and the directive could disagree. Here it is a directive on a row.
CREATE OR REPLACE FUNCTION skip(why text, how_many integer) RETURNS boolean AS $$
DECLARE
    number integer;
BEGIN
    PERFORM _require_run();
    FOR i IN 1..greatest(how_many, 0) LOOP
        number := _next_number();
        INSERT INTO pg_temp.__zapadka_assertion
            (number, kind, passed, description, directive, directive_reason)
        VALUES (number, 'skip', true, NULL, 'skip', coalesce(why, ''));
    END LOOP;
    RETURN true;
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION skip(text) RETURNS boolean AS $$
    SELECT skip($1, 1);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION skip(integer, text) RETURNS boolean AS $$
    SELECT skip($2, $1);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION skip(integer) RETURNS boolean AS $$
    SELECT skip(NULL::text, $1);
$$ LANGUAGE sql;

-- -- Scalar assertions ------------------------------------------------------

CREATE OR REPLACE FUNCTION ok(boolean, text) RETURNS boolean AS $$
    SELECT _record('ok', $1, $2);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION ok(boolean) RETURNS boolean AS $$
    SELECT _record('ok', $1, NULL);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION pass(text) RETURNS boolean AS $$
    SELECT _record('pass', true, $1);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION pass() RETURNS boolean AS $$
    SELECT _record('pass', true, NULL);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION fail(text) RETURNS boolean AS $$
    SELECT _record('fail', false, $1);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION fail() RETURNS boolean AS $$
    SELECT _record('fail', false, NULL);
$$ LANGUAGE sql;

-- `is` and `isnt` compare with IS NOT DISTINCT FROM, so NULL equals NULL.
--
-- The operands are captured *before* any text conversion, which is the whole
-- point: pgTAP compares typed records correctly and then flattens both sides to
-- text, losing exactly the structure a reader needs.
CREATE OR REPLACE FUNCTION is(anyelement, anyelement, text) RETURNS boolean AS $$
    SELECT _record(
        'is',
        NOT $1 IS DISTINCT FROM $2,
        $3,
        CASE WHEN NOT $1 IS DISTINCT FROM $2 THEN NULL ELSE jsonb_build_object(
            'comparison', 'is',
            'have', _describe($1),
            'want', _describe($2)
        ) END
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION is(anyelement, anyelement) RETURNS boolean AS $$
    SELECT is($1, $2, NULL::text);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION isnt(anyelement, anyelement, text) RETURNS boolean AS $$
    SELECT _record(
        'isnt',
        $1 IS DISTINCT FROM $2,
        $3,
        CASE WHEN $1 IS DISTINCT FROM $2 THEN NULL ELSE jsonb_build_object(
            'comparison', 'isnt',
            'have', _describe($1),
            'unwanted', _describe($2)
        ) END
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION isnt(anyelement, anyelement) RETURNS boolean AS $$
    SELECT isnt($1, $2, NULL::text);
$$ LANGUAGE sql;

-- Pattern comparisons. The pattern is inherently textual; the subject is not,
-- so it is still captured with its type.
CREATE OR REPLACE FUNCTION matches(anyelement, text, text) RETURNS boolean AS $$
    SELECT _record(
        'matches',
        $1::text ~ $2,
        $3,
        CASE WHEN $1::text ~ $2 THEN NULL ELSE jsonb_build_object(
            'comparison', 'matches', 'have', _describe($1), 'pattern', $2
        ) END
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION matches(anyelement, text) RETURNS boolean AS $$
    SELECT matches($1, $2, NULL::text);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION imatches(anyelement, text, text) RETURNS boolean AS $$
    SELECT _record(
        'imatches',
        $1::text ~* $2,
        $3,
        CASE WHEN $1::text ~* $2 THEN NULL ELSE jsonb_build_object(
            'comparison', 'imatches', 'have', _describe($1), 'pattern', $2
        ) END
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION imatches(anyelement, text) RETURNS boolean AS $$
    SELECT imatches($1, $2, NULL::text);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION doesnt_match(anyelement, text, text) RETURNS boolean AS $$
    SELECT _record(
        'doesnt_match',
        NOT ($1::text ~ $2),
        $3,
        CASE WHEN NOT ($1::text ~ $2) THEN NULL ELSE jsonb_build_object(
            'comparison', 'doesnt_match', 'have', _describe($1), 'pattern', $2
        ) END
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION doesnt_match(anyelement, text) RETURNS boolean AS $$
    SELECT doesnt_match($1, $2, NULL::text);
$$ LANGUAGE sql;

-- `cmp_ok` compares with an operator named at runtime.
--
-- The operator is validated against the catalogue before it is interpolated,
-- because it cannot be a parameter and a test file is not necessarily written
-- by someone the database should trust with arbitrary SQL.
CREATE OR REPLACE FUNCTION cmp_ok(anyelement, text, anyelement, text)
RETURNS boolean AS $$
DECLARE
    result  boolean;
    op_name name;
BEGIN
    -- Existence is checked, then PostgreSQL resolves the call itself.
    --
    -- An earlier version picked a schema with LIMIT 1 and pinned the operator
    -- there. That ignores the operands: `<` in one schema and `<` in another
    -- are different operators, and for an overloaded name the arbitrary one
    -- wins. Letting PostgreSQL resolve also gets implicit casts right, which
    -- pinning cannot -- comparing an integer with a numeric needs the cast the
    -- planner would apply.
    --
    -- The symbol is still looked up rather than trusted, because it cannot be
    -- quoted as an identifier: OPERATOR("=") is not valid syntax, so the only
    -- protection available is that the string came from the catalogue.
    SELECT o.oprname INTO op_name
      FROM pg_catalog.pg_operator o
     WHERE o.oprname = $2
       AND pg_catalog.pg_operator_is_visible(o.oid)
     LIMIT 1;

    IF op_name IS NULL THEN
        RAISE EXCEPTION 'no operator named % is visible', $2
            USING HINT = 'cmp_ok takes an operator such as ''='', ''<'' or ''@>''';
    END IF;
    EXECUTE format('SELECT $1 %s $2', op_name) INTO result USING $1, $3;
    RETURN _record(
        'cmp_ok',
        coalesce(result, false),
        $4,
        CASE WHEN coalesce(result, false) THEN NULL ELSE jsonb_build_object(
            'comparison', 'cmp_ok',
            'operator', $2,
            'have', _describe($1),
            'want', _describe($3)
        ) END
    );
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION cmp_ok(anyelement, text, anyelement) RETURNS boolean AS $$
    SELECT cmp_ok($1, $2, $3, NULL::text);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION isa_ok(anyelement, regtype, text) RETURNS boolean AS $$
    SELECT _record(
        'isa_ok',
        pg_typeof($1) = $2,
        $3,
        CASE WHEN pg_typeof($1) = $2 THEN NULL ELSE jsonb_build_object(
            'comparison', 'isa_ok',
            'have', pg_typeof($1)::text,
            'want', $2::text
        ) END
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION isa_ok(anyelement, regtype) RETURNS boolean AS $$
    SELECT isa_ok($1, $2, NULL::text);
$$ LANGUAGE sql;
