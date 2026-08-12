-- Assertions that compare one query's rows against another's.
--
-- This is the family where pgTAP is weakest and where structure pays. pgTAP
-- compares typed records correctly and then renders both sides with
-- `record::text`, so a reader gets two opaque strings and has to spot the
-- difference by eye. Here the comparison is still done by PostgreSQL on real
-- values, but what gets *reported* keeps the rows, the column names and the
-- column types.
--
-- Four engines, not fifty:
--
--   ordered      results_eq / results_ne  -- position by position
--   set          set_eq / set_ne / set_has / set_hasnt  -- duplicates ignored
--   bag          bag_eq / bag_ne / bag_has / bag_hasnt  -- duplicates counted
--   emptiness    is_empty / isnt_empty
--
-- Everything else in the family is an overload over one of them.
--
-- Truth comes from `EXCEPT` and `EXCEPT ALL` on the real values. JSON is only
-- ever used to *report* a mismatch, so a passing comparison serialises nothing
-- and costs nothing.

-- How many differing rows a failure carries. Enough to see the shape of the
-- problem; not so many that one bad assertion buries the report.
CREATE OR REPLACE FUNCTION _sample_limit() RETURNS integer AS $$
    SELECT 20;
$$ LANGUAGE sql IMMUTABLE;

-- Drops a scratch relation if it is there.
--
-- `DROP TABLE IF EXISTS` would do, but it raises a NOTICE when the table is
-- absent -- which is the normal case for the first comparison in a file, and
-- noise in a test run nobody should have to learn to ignore.
CREATE OR REPLACE FUNCTION _drop_scratch(relation name) RETURNS void AS $$
BEGIN
    IF to_regclass('pg_temp.' || quote_ident(relation)) IS NOT NULL THEN
        EXECUTE format('DROP TABLE pg_temp.%I', relation);
    END IF;
END;
$$ LANGUAGE plpgsql;

-- Materialises a query into a named temporary relation.
--
-- Each input query runs exactly once. A test query may be volatile or
-- expensive, and re-running it to produce diagnostics could report a
-- difference that never existed.
CREATE OR REPLACE FUNCTION _materialise(relation name, query text)
RETURNS void AS $$
BEGIN
    PERFORM _drop_scratch(relation);
    EXECUTE format('CREATE TEMP TABLE %I ON COMMIT DROP AS %s', relation, query);
END;
$$ LANGUAGE plpgsql;

-- The same, for an array of expected values: one row per element.
CREATE OR REPLACE FUNCTION _materialise_array(relation name, values_ anyarray)
RETURNS void AS $$
BEGIN
    PERFORM _drop_scratch(relation);
    EXECUTE format('CREATE TEMP TABLE %I ON COMMIT DROP AS SELECT unnest($1) AS value',
                   relation)
    USING values_;
END;
$$ LANGUAGE plpgsql;

-- The column names of a materialised relation, in order.
CREATE OR REPLACE FUNCTION _column_names(relation name) RETURNS name[] AS $$
    SELECT array_agg(a.attname ORDER BY a.attnum)
      FROM pg_catalog.pg_attribute a
     WHERE a.attrelid = ('pg_temp.' || quote_ident(relation))::regclass
       AND a.attnum > 0
       AND NOT a.attisdropped;
$$ LANGUAGE sql STABLE;

-- A description of a relation's columns: position, name and type.
CREATE OR REPLACE FUNCTION _column_descriptors(relation name) RETURNS jsonb AS $$
    SELECT coalesce(jsonb_agg(
        jsonb_build_object(
            'position', a.attnum,
            'name',     a.attname,
            'type',     pg_catalog.format_type(a.atttypid, a.atttypmod)
        ) ORDER BY a.attnum
    ), '[]'::jsonb)
      FROM pg_catalog.pg_attribute a
     WHERE a.attrelid = ('pg_temp.' || quote_ident(relation))::regclass
       AND a.attnum > 0
       AND NOT a.attisdropped;
$$ LANGUAGE sql STABLE;

-- Rows of a relation as JSON arrays, bounded.
--
-- Arrays rather than objects on purpose: column *order* is part of what is
-- being compared, and a query may produce duplicate or absent column labels
-- that an object cannot represent. The descriptors say what each position is.
CREATE OR REPLACE FUNCTION _sample(relation name, sample_limit integer)
RETURNS jsonb AS $$
DECLARE
    columns name[];
    builder text;
    result  jsonb;
BEGIN
    columns := _column_names(relation);
    IF columns IS NULL THEN
        RETURN '[]'::jsonb;
    END IF;

    SELECT string_agg(format('to_jsonb(%I)', c), ', ')
      INTO builder
      FROM unnest(columns) AS c;

    EXECUTE format(
        'SELECT coalesce(jsonb_agg(row_json), ''[]''::jsonb)
           FROM (SELECT jsonb_build_array(%s) AS row_json
                   FROM pg_temp.%I LIMIT %s) sampled',
        builder, relation, sample_limit
    ) INTO result;
    RETURN result;
END;
$$ LANGUAGE plpgsql;

-- Rows present in `left_rel` but not in `right_rel`, materialised.
--
-- `all_rows` selects `EXCEPT ALL` over `EXCEPT`, which is the whole difference
-- between bag and set semantics: with it, three copies against one leaves two.
CREATE OR REPLACE FUNCTION _difference(
    target    name,
    left_rel  name,
    right_rel name,
    all_rows  boolean
) RETURNS void AS $$
BEGIN
    PERFORM _drop_scratch(target);
    EXECUTE format(
        'CREATE TEMP TABLE %I ON COMMIT DROP AS
             SELECT * FROM pg_temp.%I %s SELECT * FROM pg_temp.%I',
        target, left_rel, CASE WHEN all_rows THEN 'EXCEPT ALL' ELSE 'EXCEPT' END,
        right_rel
    );
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION _count_of(relation name) RETURNS bigint AS $$
DECLARE
    total bigint;
BEGIN
    EXECUTE format('SELECT count(*) FROM pg_temp.%I', relation) INTO total;
    RETURN total;
END;
$$ LANGUAGE plpgsql;

-- Compares two already-materialised relations and records the result.
--
-- `mode` is 'set' or 'bag'; `want_subset` asks only that everything expected is
-- present, which is what the `_has` variants mean.
CREATE OR REPLACE FUNCTION _compare_unordered(
    kind        text,
    mode        text,
    want_subset boolean,
    description text
) RETURNS boolean AS $$
DECLARE
    all_rows     boolean := (mode = 'bag');
    missing      bigint;
    extra        bigint := 0;
    passed       boolean;
    detail       jsonb;
BEGIN
    -- A shape mismatch is a real answer, not a crash. `EXCEPT` raises when the
    -- two sides have different column counts or incompatible types, and that is
    -- exactly the failure a reader most needs described.
    BEGIN
        PERFORM _difference('__zapadka_missing', '__zapadka_want', '__zapadka_have', all_rows);
        IF NOT want_subset THEN
            PERFORM _difference('__zapadka_extra', '__zapadka_have', '__zapadka_want', all_rows);
        END IF;
    EXCEPTION WHEN datatype_mismatch OR syntax_error_or_access_rule_violation THEN
        RETURN _record(kind, false, description, jsonb_build_object(
            'kind', mode,
            'problem', 'the two queries do not have comparable columns',
            'detail', SQLERRM,
            'have_columns', _column_descriptors('__zapadka_have'),
            'want_columns', _column_descriptors('__zapadka_want')
        ));
    END;

    missing := _count_of('__zapadka_missing');
    IF NOT want_subset THEN
        extra := _count_of('__zapadka_extra');
    END IF;
    passed := missing = 0 AND extra = 0;

    IF passed THEN
        RETURN _record(kind, true, description, NULL);
    END IF;

    detail := jsonb_build_object(
        'kind', mode,
        'columns', _column_descriptors('__zapadka_have'),
        'missing_count', missing,
        'extra_count', extra,
        'missing', _sample('__zapadka_missing', _sample_limit()),
        'sample_limit', _sample_limit(),
        'truncated', missing > _sample_limit() OR extra > _sample_limit()
    );
    IF NOT want_subset THEN
        detail := detail || jsonb_build_object('extra', _sample('__zapadka_extra', _sample_limit()));
    END IF;
    RETURN _record(kind, false, description, detail);
END;
$$ LANGUAGE plpgsql;

-- -- Set and bag equality ---------------------------------------------------

CREATE OR REPLACE FUNCTION set_eq(text, text, text) RETURNS boolean AS $$
BEGIN
    PERFORM _materialise('__zapadka_have', $1);
    PERFORM _materialise('__zapadka_want', $2);
    RETURN _compare_unordered('set_eq', 'set', false, $3);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION set_eq(text, anyarray, text) RETURNS boolean AS $$
BEGIN
    PERFORM _materialise('__zapadka_have', $1);
    PERFORM _materialise_array('__zapadka_want', $2);
    RETURN _compare_unordered('set_eq', 'set', false, $3);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION set_eq(text, text) RETURNS boolean AS $$
    SELECT set_eq($1, $2, NULL::text);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION set_eq(text, anyarray) RETURNS boolean AS $$
    SELECT set_eq($1, $2, NULL::text);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION bag_eq(text, text, text) RETURNS boolean AS $$
BEGIN
    PERFORM _materialise('__zapadka_have', $1);
    PERFORM _materialise('__zapadka_want', $2);
    RETURN _compare_unordered('bag_eq', 'bag', false, $3);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION bag_eq(text, anyarray, text) RETURNS boolean AS $$
BEGIN
    PERFORM _materialise('__zapadka_have', $1);
    PERFORM _materialise_array('__zapadka_want', $2);
    RETURN _compare_unordered('bag_eq', 'bag', false, $3);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION bag_eq(text, text) RETURNS boolean AS $$
    SELECT bag_eq($1, $2, NULL::text);
$$ LANGUAGE sql;

-- -- Containment ------------------------------------------------------------

CREATE OR REPLACE FUNCTION set_has(text, text, text) RETURNS boolean AS $$
BEGIN
    PERFORM _materialise('__zapadka_have', $1);
    PERFORM _materialise('__zapadka_want', $2);
    RETURN _compare_unordered('set_has', 'set', true, $3);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION set_has(text, text) RETURNS boolean AS $$
    SELECT set_has($1, $2, NULL::text);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION bag_has(text, text, text) RETURNS boolean AS $$
BEGIN
    PERFORM _materialise('__zapadka_have', $1);
    PERFORM _materialise('__zapadka_want', $2);
    RETURN _compare_unordered('bag_has', 'bag', true, $3);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION bag_has(text, text) RETURNS boolean AS $$
    SELECT bag_has($1, $2, NULL::text);
$$ LANGUAGE sql;

-- -- Inequality -------------------------------------------------------------
--
-- Deliberately thin: "these differ somehow" needs no diagnostic beyond the fact
-- that they did not, because the interesting case is the one where they are
-- equal and the author expected otherwise.

CREATE OR REPLACE FUNCTION set_ne(text, text, text) RETURNS boolean AS $$
DECLARE
    same boolean;
BEGIN
    PERFORM _materialise('__zapadka_have', $1);
    PERFORM _materialise('__zapadka_want', $2);

    -- Incomparable shapes are a failed assertion, not an aborted file. `set_eq`
    -- already treats them that way, and a file that stops at the first
    -- mismatched query reports nothing about the assertions after it.
    BEGIN
        PERFORM _difference('__zapadka_missing', '__zapadka_want', '__zapadka_have', false);
        PERFORM _difference('__zapadka_extra', '__zapadka_have', '__zapadka_want', false);
    EXCEPTION WHEN datatype_mismatch OR syntax_error_or_access_rule_violation THEN
        -- Queries that cannot be compared are certainly not the same set, so
        -- this is the one comparison where an incomparable shape is a pass.
        RETURN _record('set_ne', true, $3, NULL);
    END;

    same := _count_of('__zapadka_missing') = 0 AND _count_of('__zapadka_extra') = 0;
    RETURN _record('set_ne', NOT same, $3,
        CASE WHEN same THEN jsonb_build_object(
            'kind', 'set', 'problem', 'the two queries produced the same set'
        ) END);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION set_ne(text, text) RETURNS boolean AS $$
    SELECT set_ne($1, $2, NULL::text);
$$ LANGUAGE sql;

-- -- Ordered comparison -----------------------------------------------------

-- Materialises a query preserving the order its rows arrived in.
--
-- `row_number() OVER ()` numbers rows in the order the scan produces them,
-- which is the order the query actually returned. That is the only order an
-- ordered comparison can honestly mean: a query without `ORDER BY` has no
-- guaranteed order, and this reports what it did rather than what it promised.
CREATE OR REPLACE FUNCTION _materialise_ordered(relation name, query text)
RETURNS void AS $$
BEGIN
    PERFORM _drop_scratch(relation);
    EXECUTE format(
        'CREATE TEMP TABLE %I ON COMMIT DROP AS
             SELECT row_number() OVER () AS __position, source.*
               FROM (%s) source',
        relation, query
    );
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION results_eq(text, text, text) RETURNS boolean AS $$
DECLARE
    have_count bigint;
    want_count bigint;
    first_bad  bigint;
    predicate  text;
    detail     jsonb;
BEGIN
    PERFORM _materialise_ordered('__zapadka_have', $1);
    PERFORM _materialise_ordered('__zapadka_want', $2);

    have_count := _count_of('__zapadka_have');
    want_count := _count_of('__zapadka_want');

    -- Compared with IS DISTINCT FROM across the whole row, so NULL equals NULL
    -- -- which is what a test means by "the same row", and what plain `=` would
    -- get wrong.
    -- Paired by position, not by name. `results_eq` compares row *values*; two
    -- queries producing the same values under different aliases are equal, and
    -- naming both sides from the left query's columns would look for a column
    -- the right side does not have.
    BEGIN
        SELECT string_agg(
                   format('h.%I IS DISTINCT FROM w.%I', pair.have_col, pair.want_col),
                   ' OR ' ORDER BY pair.position
               )
          INTO predicate
          FROM (
              SELECT h.position, h.column_name AS have_col, w.column_name AS want_col
                FROM unnest(_column_names('__zapadka_have'))
                     WITH ORDINALITY AS h(column_name, position)
                JOIN unnest(_column_names('__zapadka_want'))
                     WITH ORDINALITY AS w(column_name, position)
                  ON w.position = h.position
               WHERE h.column_name <> '__position'
          ) AS pair;

        -- A differing column count is a real difference, not a comparison to
        -- attempt: pairing would silently ignore the extra columns.
        IF array_length(_column_names('__zapadka_have'), 1)
           IS DISTINCT FROM array_length(_column_names('__zapadka_want'), 1) THEN
            RETURN _record('results_eq', false, $3, jsonb_build_object(
                'kind', 'ordered',
                'problem', 'the two queries return different numbers of columns',
                'have_columns', _column_descriptors('__zapadka_have'),
                'want_columns', _column_descriptors('__zapadka_want')
            ));
        END IF;

        EXECUTE format(
            'SELECT min(h.__position)
               FROM pg_temp.__zapadka_have h
               FULL JOIN pg_temp.__zapadka_want w ON w.__position = h.__position
              WHERE h.__position IS NULL OR w.__position IS NULL OR (%s)',
            coalesce(predicate, 'false')
        ) INTO first_bad;
    EXCEPTION WHEN undefined_column OR datatype_mismatch OR undefined_function THEN
        RETURN _record('results_eq', false, $3, jsonb_build_object(
            'kind', 'ordered',
            'problem', 'the two queries do not have comparable columns',
            'detail', SQLERRM,
            'have_columns', _column_descriptors('__zapadka_have'),
            'want_columns', _column_descriptors('__zapadka_want')
        ));
    END;

    IF first_bad IS NULL AND have_count = want_count THEN
        RETURN _record('results_eq', true, $3, NULL);
    END IF;

    detail := jsonb_build_object(
        'kind', 'ordered',
        'columns', _column_descriptors('__zapadka_have'),
        'have_row_count', have_count,
        'want_row_count', want_count,
        'first_difference_at', coalesce(first_bad, least(have_count, want_count) + 1)
    );
    RETURN _record('results_eq', false, $3, detail);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION results_eq(text, anyarray, text) RETURNS boolean AS $$
    SELECT results_eq($1, 'SELECT unnest(' || quote_literal($2::text) || '::'
                          || pg_typeof($2)::text || ')', $3);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION results_eq(text, text) RETURNS boolean AS $$
    SELECT results_eq($1, $2, NULL::text);
$$ LANGUAGE sql;

-- -- Emptiness --------------------------------------------------------------

CREATE OR REPLACE FUNCTION is_empty(text, text) RETURNS boolean AS $$
DECLARE
    total bigint;
BEGIN
    PERFORM _materialise('__zapadka_have', $1);
    total := _count_of('__zapadka_have');
    RETURN _record('is_empty', total = 0, $2,
        CASE WHEN total = 0 THEN NULL ELSE jsonb_build_object(
            'kind', 'emptiness',
            'columns', _column_descriptors('__zapadka_have'),
            'row_count', total,
            'rows', _sample('__zapadka_have', _sample_limit()),
            'sample_limit', _sample_limit(),
            'truncated', total > _sample_limit()
        ) END);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION is_empty(text) RETURNS boolean AS $$
    SELECT is_empty($1, NULL::text);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION isnt_empty(text, text) RETURNS boolean AS $$
DECLARE
    total bigint;
BEGIN
    PERFORM _materialise('__zapadka_have', $1);
    total := _count_of('__zapadka_have');
    RETURN _record('isnt_empty', total > 0, $2,
        CASE WHEN total > 0 THEN NULL ELSE jsonb_build_object(
            'kind', 'emptiness',
            'columns', _column_descriptors('__zapadka_have'),
            'row_count', 0
        ) END);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION isnt_empty(text) RETURNS boolean AS $$
    SELECT isnt_empty($1, NULL::text);
$$ LANGUAGE sql;
