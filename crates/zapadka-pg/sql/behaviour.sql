-- Assertions about what running some SQL does: whether it raises, and how long
-- it takes.
--
-- An exception carries five separate facts -- SQLSTATE, message, detail, hint,
-- and where it came from -- and pgTAP flattens the interesting ones into a
-- sentence. Keeping them apart is the difference between a report that says
-- "the error text did not match" and one that says "you expected 23505 and got
-- 23503".
--
-- `EXECUTE` inside a `BEGIN ... EXCEPTION` block opens a subtransaction, so
-- anything the statement did is rolled back when it raises. The assertion rows
-- live in the outer transaction and survive.

-- Records the outcome of a statement that was expected to raise.
CREATE OR REPLACE FUNCTION _record_throw(
    kind             text,
    matched          boolean,
    description      text,
    expectation      jsonb,
    caught           jsonb
) RETURNS boolean AS $$
    SELECT _record(
        kind,
        matched,
        description,
        CASE WHEN matched THEN NULL
             ELSE jsonb_build_object('kind', 'exception')
                  || jsonb_build_object('expected', expectation)
                  || jsonb_build_object('caught', caught)
        END
    );
$$ LANGUAGE sql;

-- `throws_ok(sql, sqlstate, message, description)`
--
-- A NULL expectation is "anything": `throws_ok(sql)` asserts only that
-- something was raised.
CREATE OR REPLACE FUNCTION throws_ok(text, text, text, text) RETURNS boolean AS $$
DECLARE
    code    text;
    message text;
    detail  text;
    hint    text;
    caught  jsonb;
    matched boolean;
BEGIN
    BEGIN
        EXECUTE $1;
    EXCEPTION WHEN OTHERS THEN
        GET STACKED DIAGNOSTICS
            code    = RETURNED_SQLSTATE,
            message = MESSAGE_TEXT,
            detail  = PG_EXCEPTION_DETAIL,
            hint    = PG_EXCEPTION_HINT;
        caught := jsonb_build_object(
            'sqlstate', code,
            'message',  message,
            'detail',   nullif(detail, ''),
            'hint',     nullif(hint, '')
        );
        matched := (($2 IS NULL) OR code = $2)
               AND (($3 IS NULL) OR message = $3);
        RETURN _record_throw('throws_ok', matched, $4,
            jsonb_build_object('sqlstate', $2, 'message', $3), caught);
    END;

    -- Falling out of the block means nothing was raised, which for this
    -- assertion is the failure.
    RETURN _record_throw('throws_ok', false, $4,
        jsonb_build_object('sqlstate', $2, 'message', $3),
        jsonb_build_object('problem', 'the statement completed without raising'));
END;
$$ LANGUAGE plpgsql;

-- The shorter forms are ambiguous by design, and pgTAP resolves the ambiguity
-- by length: a five-byte second argument is a SQLSTATE, anything else is the
-- expected message. Guessing differently would make
-- `throws_ok(sql, 'some message')` fail as a malformed SQLSTATE, and
-- `throws_ok(sql, '23505', 'expected message')` pass while ignoring the
-- message entirely.
CREATE OR REPLACE FUNCTION throws_ok(text, text, text) RETURNS boolean AS $$
    SELECT CASE
        WHEN octet_length($2) = 5 THEN throws_ok($1, $2, $3, NULL::text)
        ELSE throws_ok($1, NULL::text, $2, $3)
    END;
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION throws_ok(text, text) RETURNS boolean AS $$
    SELECT CASE
        WHEN octet_length($2) = 5 THEN throws_ok($1, $2, NULL::text, NULL::text)
        ELSE throws_ok($1, NULL::text, $2, NULL::text)
    END;
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION throws_ok(text) RETURNS boolean AS $$
    SELECT throws_ok($1, NULL::text, NULL::text, NULL::text);
$$ LANGUAGE sql;

-- `throws_like(sql, pattern, description)` -- the message must match a LIKE
-- pattern. Case-sensitive; `throws_ilike` is not.
CREATE OR REPLACE FUNCTION _throws_matching(
    kind        text,
    query       text,
    pattern     text,
    description text,
    insensitive boolean
) RETURNS boolean AS $$
DECLARE
    code    text;
    message text;
    detail  text;
    hint    text;
    matched boolean;
BEGIN
    BEGIN
        EXECUTE query;
    EXCEPTION WHEN OTHERS THEN
        GET STACKED DIAGNOSTICS
            code    = RETURNED_SQLSTATE,
            message = MESSAGE_TEXT,
            detail  = PG_EXCEPTION_DETAIL,
            hint    = PG_EXCEPTION_HINT;
        matched := CASE WHEN insensitive THEN message ILIKE pattern
                        ELSE message LIKE pattern END;
        RETURN _record_throw(kind, matched, description,
            jsonb_build_object('message_like', pattern),
            jsonb_build_object(
                'sqlstate', code,
                'message',  message,
                'detail',   nullif(detail, ''),
                'hint',     nullif(hint, '')
            ));
    END;

    RETURN _record_throw(kind, false, description,
        jsonb_build_object('message_like', pattern),
        jsonb_build_object('problem', 'the statement completed without raising'));
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION throws_like(text, text, text) RETURNS boolean AS $$
    SELECT _throws_matching('throws_like', $1, $2, $3, false);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION throws_like(text, text) RETURNS boolean AS $$
    SELECT _throws_matching('throws_like', $1, $2, NULL::text, false);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION throws_ilike(text, text, text) RETURNS boolean AS $$
    SELECT _throws_matching('throws_ilike', $1, $2, $3, true);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION throws_ilike(text, text) RETURNS boolean AS $$
    SELECT _throws_matching('throws_ilike', $1, $2, NULL::text, true);
$$ LANGUAGE sql;

-- `throws_matching(sql, regex, description)` -- the message must match a
-- regular expression, which is often what you want when the interesting part of
-- an error is buried mid-sentence.
CREATE OR REPLACE FUNCTION throws_matching(text, text, text) RETURNS boolean AS $$
DECLARE
    code    text;
    message text;
BEGIN
    BEGIN
        EXECUTE $1;
    EXCEPTION WHEN OTHERS THEN
        GET STACKED DIAGNOSTICS code = RETURNED_SQLSTATE, message = MESSAGE_TEXT;
        RETURN _record_throw('throws_matching', message ~ $2, $3,
            jsonb_build_object('message_matches', $2),
            jsonb_build_object('sqlstate', code, 'message', message));
    END;
    RETURN _record_throw('throws_matching', false, $3,
        jsonb_build_object('message_matches', $2),
        jsonb_build_object('problem', 'the statement completed without raising'));
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION throws_matching(text, text) RETURNS boolean AS $$
    SELECT throws_matching($1, $2, NULL::text);
$$ LANGUAGE sql;

-- `lives_ok(sql, description)` -- the statement must not raise.
--
-- The mirror of `throws_ok`, and the more useful of the two in a privilege
-- test: it captures the error that was not supposed to happen, with its
-- SQLSTATE, rather than reporting only that something went wrong.
CREATE OR REPLACE FUNCTION lives_ok(text, text) RETURNS boolean AS $$
DECLARE
    code    text;
    message text;
    detail  text;
    hint    text;
BEGIN
    BEGIN
        EXECUTE $1;
    EXCEPTION WHEN OTHERS THEN
        GET STACKED DIAGNOSTICS
            code    = RETURNED_SQLSTATE,
            message = MESSAGE_TEXT,
            detail  = PG_EXCEPTION_DETAIL,
            hint    = PG_EXCEPTION_HINT;
        RETURN _record('lives_ok', false, $2, jsonb_build_object(
            'kind', 'exception',
            'caught', jsonb_build_object(
                'sqlstate', code,
                'message',  message,
                'detail',   nullif(detail, ''),
                'hint',     nullif(hint, '')
            )
        ));
    END;
    RETURN _record('lives_ok', true, $2, NULL);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION lives_ok(text) RETURNS boolean AS $$
    SELECT lives_ok($1, NULL::text);
$$ LANGUAGE sql;

-- -- Performance -------------------------------------------------------------
--
-- Timing assertions are inherently flaky on shared hardware, so the detail
-- reports what was actually observed rather than only that a bound was missed.
-- A reader can then tell "twice the budget" from "a hundred times the budget".

CREATE OR REPLACE FUNCTION performs_ok(text, numeric, text) RETURNS boolean AS $$
DECLARE
    started timestamptz;
    elapsed numeric;
BEGIN
    started := clock_timestamp();
    EXECUTE $1;
    elapsed := extract(epoch FROM clock_timestamp() - started) * 1000;
    RETURN _record('performs_ok', elapsed < $2, $3,
        CASE WHEN elapsed < $2 THEN NULL ELSE jsonb_build_object(
            'kind', 'timing',
            'budget_ms', $2,
            'observed_ms', round(elapsed, 3)
        ) END);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION performs_ok(text, numeric) RETURNS boolean AS $$
    SELECT performs_ok($1, $2, NULL::text);
$$ LANGUAGE sql;

-- `performs_within(sql, expected_ms, tolerance_ms, iterations, description)`
--
-- Runs the statement repeatedly and compares the mean against a window. More
-- honest than a single timing for anything short, where scheduling noise
-- dominates.
CREATE OR REPLACE FUNCTION performs_within(text, numeric, numeric, integer, text)
RETURNS boolean AS $$
DECLARE
    started timestamptz;
    total   numeric := 0;
    mean    numeric;
BEGIN
    FOR i IN 1..greatest($4, 1) LOOP
        started := clock_timestamp();
        EXECUTE $1;
        total := total + extract(epoch FROM clock_timestamp() - started) * 1000;
    END LOOP;
    mean := total / greatest($4, 1);
    RETURN _record('performs_within', abs(mean - $2) <= $3, $5,
        CASE WHEN abs(mean - $2) <= $3 THEN NULL ELSE jsonb_build_object(
            'kind', 'timing',
            'expected_ms', $2,
            'tolerance_ms', $3,
            'iterations', greatest($4, 1),
            'mean_ms', round(mean, 3)
        ) END);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION performs_within(text, numeric, numeric, text)
RETURNS boolean AS $$
    SELECT performs_within($1, $2, $3, 10, $4);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION performs_within(text, numeric, numeric, integer)
RETURNS boolean AS $$
    SELECT performs_within($1, $2, $3, $4, NULL::text);
$$ LANGUAGE sql;
