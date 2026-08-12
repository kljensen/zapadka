-- Assertions about which objects exist.
--
-- These are thin: a catalogue predicate returning boolean, wrapped by a
-- one-line assertion. Keeping the predicate separate from the assertion is what
-- pgTAP does too, and it is the right shape -- the hard part is the catalogue
-- query, and it should be readable on its own.
--
-- Overloads follow pgTAP's disambiguation, which relies on `name` versus
-- `text`: `has_table(name, name)` is (schema, table), while
-- `has_table(name, text)` is (table, description). It is subtle, but it is the
-- convention every pgTAP user already has in their fingers.

-- -- Catalogue predicates ---------------------------------------------------

-- Whether a relation of one of `kinds` exists in a named schema.
--
-- `kinds` are `pg_class.relkind` values: 'r' ordinary table, 'p' partitioned,
-- 'v' view, 'm' materialised view, 'i' index, 'S' sequence, 'f' foreign table.
CREATE OR REPLACE FUNCTION _relation_exists(
    kinds         "char"[],
    schema_name   name,
    relation_name name
) RETURNS boolean AS $$
    SELECT EXISTS (
        SELECT 1
          FROM pg_catalog.pg_class c
          JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         WHERE c.relname = relation_name
           AND n.nspname = schema_name
           AND c.relkind = ANY(kinds)
    );
$$ LANGUAGE sql STABLE;

-- The same, for a relation visible on the current search path.
--
-- Visibility rather than "any schema": an unqualified name in a test means the
-- one the session would actually resolve, and a match in a schema the session
-- cannot see would be a false pass.
CREATE OR REPLACE FUNCTION _relation_exists(
    kinds         "char"[],
    relation_name name
) RETURNS boolean AS $$
    SELECT EXISTS (
        SELECT 1
          FROM pg_catalog.pg_class c
         WHERE c.relname = relation_name
           AND c.relkind = ANY(kinds)
           AND pg_catalog.pg_table_is_visible(c.oid)
    );
$$ LANGUAGE sql STABLE;

CREATE OR REPLACE FUNCTION _column_exists(
    schema_name name,
    table_name  name,
    column_name name
) RETURNS boolean AS $$
    SELECT EXISTS (
        SELECT 1
          FROM pg_catalog.pg_attribute a
          JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
          JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         WHERE n.nspname = schema_name
           AND c.relname = table_name
           AND a.attname = column_name
           -- System columns are negative; a dropped column keeps its row.
           AND a.attnum > 0
           AND NOT a.attisdropped
    );
$$ LANGUAGE sql STABLE;

CREATE OR REPLACE FUNCTION _column_exists(
    table_name  name,
    column_name name
) RETURNS boolean AS $$
    SELECT EXISTS (
        SELECT 1
          FROM pg_catalog.pg_attribute a
          JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
         WHERE c.relname = table_name
           AND a.attname = column_name
           AND a.attnum > 0
           AND NOT a.attisdropped
           AND pg_catalog.pg_table_is_visible(c.oid)
    );
$$ LANGUAGE sql STABLE;

-- -- Schemas ----------------------------------------------------------------

CREATE OR REPLACE FUNCTION has_schema(name, text) RETURNS boolean AS $$
    SELECT _record(
        'has_schema',
        EXISTS (SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = $1),
        $2,
        jsonb_build_object('schema', $1)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_schema(name) RETURNS boolean AS $$
    SELECT has_schema($1, 'schema ' || quote_ident($1) || ' should exist');
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_schema(name, text) RETURNS boolean AS $$
    SELECT _record(
        'hasnt_schema',
        NOT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = $1),
        $2,
        jsonb_build_object('schema', $1)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_schema(name) RETURNS boolean AS $$
    SELECT hasnt_schema($1, 'schema ' || quote_ident($1) || ' should not exist');
$$ LANGUAGE sql;

-- -- Tables -----------------------------------------------------------------
--
-- '{r,p}': a partitioned table is a table. A test asserting `has_table` should
-- not start failing because someone partitioned it.

CREATE OR REPLACE FUNCTION has_table(name, name, text) RETURNS boolean AS $$
    SELECT _record(
        'has_table',
        _relation_exists('{r,p}'::"char"[], $1, $2),
        $3,
        jsonb_build_object('schema', $1, 'table', $2)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_table(name, name) RETURNS boolean AS $$
    SELECT has_table($1, $2,
        'table ' || quote_ident($1) || '.' || quote_ident($2) || ' should exist');
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_table(name, text) RETURNS boolean AS $$
    SELECT _record(
        'has_table',
        _relation_exists('{r,p}'::"char"[], $1),
        $2,
        jsonb_build_object('table', $1)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_table(name) RETURNS boolean AS $$
    SELECT has_table($1, 'table ' || quote_ident($1) || ' should exist');
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_table(name, name, text) RETURNS boolean AS $$
    SELECT _record(
        'hasnt_table',
        NOT _relation_exists('{r,p}'::"char"[], $1, $2),
        $3,
        jsonb_build_object('schema', $1, 'table', $2)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_table(name, text) RETURNS boolean AS $$
    SELECT _record(
        'hasnt_table',
        NOT _relation_exists('{r,p}'::"char"[], $1),
        $2,
        jsonb_build_object('table', $1)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_table(name) RETURNS boolean AS $$
    SELECT hasnt_table($1, 'table ' || quote_ident($1) || ' should not exist');
$$ LANGUAGE sql;

-- -- Views ------------------------------------------------------------------
--
-- '{v,m}': a materialised view is a view for the purpose of "does it exist".

CREATE OR REPLACE FUNCTION has_view(name, name, text) RETURNS boolean AS $$
    SELECT _record(
        'has_view',
        _relation_exists('{v,m}'::"char"[], $1, $2),
        $3,
        jsonb_build_object('schema', $1, 'view', $2)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_view(name, name) RETURNS boolean AS $$
    SELECT has_view($1, $2,
        'view ' || quote_ident($1) || '.' || quote_ident($2) || ' should exist');
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_view(name, text) RETURNS boolean AS $$
    SELECT _record(
        'has_view',
        _relation_exists('{v,m}'::"char"[], $1),
        $2,
        jsonb_build_object('view', $1)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_view(name) RETURNS boolean AS $$
    SELECT has_view($1, 'view ' || quote_ident($1) || ' should exist');
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_view(name, text) RETURNS boolean AS $$
    SELECT _record(
        'hasnt_view',
        NOT _relation_exists('{v,m}'::"char"[], $1),
        $2,
        jsonb_build_object('view', $1)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_view(name) RETURNS boolean AS $$
    SELECT hasnt_view($1, 'view ' || quote_ident($1) || ' should not exist');
$$ LANGUAGE sql;

-- -- Sequences --------------------------------------------------------------

CREATE OR REPLACE FUNCTION has_sequence(name, name, text) RETURNS boolean AS $$
    SELECT _record(
        'has_sequence',
        _relation_exists('{S}'::"char"[], $1, $2),
        $3,
        jsonb_build_object('schema', $1, 'sequence', $2)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_sequence(name, text) RETURNS boolean AS $$
    SELECT _record(
        'has_sequence',
        _relation_exists('{S}'::"char"[], $1),
        $2,
        jsonb_build_object('sequence', $1)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_sequence(name) RETURNS boolean AS $$
    SELECT has_sequence($1, 'sequence ' || quote_ident($1) || ' should exist');
$$ LANGUAGE sql;

-- -- Columns ----------------------------------------------------------------

CREATE OR REPLACE FUNCTION has_column(name, name, name, text) RETURNS boolean AS $$
    SELECT _record(
        'has_column',
        _column_exists($1, $2, $3),
        $4,
        jsonb_build_object('schema', $1, 'table', $2, 'column', $3)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_column(name, name, name) RETURNS boolean AS $$
    SELECT has_column($1, $2, $3,
        'column ' || quote_ident($1) || '.' || quote_ident($2) || '.'
                  || quote_ident($3) || ' should exist');
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_column(name, name, text) RETURNS boolean AS $$
    SELECT _record(
        'has_column',
        _column_exists($1, $2),
        $3,
        jsonb_build_object('table', $1, 'column', $2)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_column(name, name) RETURNS boolean AS $$
    SELECT has_column($1, $2,
        'column ' || quote_ident($1) || '.' || quote_ident($2) || ' should exist');
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_column(name, name, text) RETURNS boolean AS $$
    SELECT _record(
        'hasnt_column',
        NOT _column_exists($1, $2),
        $3,
        jsonb_build_object('table', $1, 'column', $2)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_column(name, name) RETURNS boolean AS $$
    SELECT hasnt_column($1, $2,
        'column ' || quote_ident($1) || '.' || quote_ident($2) || ' should not exist');
$$ LANGUAGE sql;

-- -- Keys and constraints ---------------------------------------------------

-- The primary-key columns of a table, in key order.
--
-- Ordered by position within the constraint rather than by column number: a
-- composite key on (b, a) is not the same key as (a, b), and reporting them
-- sorted would hide a real difference.
CREATE OR REPLACE FUNCTION _pk_columns(schema_name name, table_name name)
RETURNS name[] AS $$
    SELECT array_agg(a.attname ORDER BY k.ord)
      FROM pg_catalog.pg_constraint c
      JOIN pg_catalog.pg_class t ON t.oid = c.conrelid
      JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace
      CROSS JOIN LATERAL unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord)
      JOIN pg_catalog.pg_attribute a
        ON a.attrelid = t.oid AND a.attnum = k.attnum
     WHERE c.contype = 'p'
       AND n.nspname = schema_name
       AND t.relname = table_name;
$$ LANGUAGE sql STABLE;

CREATE OR REPLACE FUNCTION _pk_columns(table_name name)
RETURNS name[] AS $$
    SELECT _pk_columns(n.nspname, table_name)
      FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     WHERE c.relname = table_name
       AND pg_catalog.pg_table_is_visible(c.oid)
     LIMIT 1;
$$ LANGUAGE sql STABLE;

-- Records a primary-key comparison, reporting both key column lists.
--
-- The detail carries the actual and expected column arrays rather than a
-- rendered sentence, so a reader can be shown which column is missing instead
-- of comparing two strings by eye.
CREATE OR REPLACE FUNCTION _record_pk(
    actual   name[],
    expected name[],
    description text,
    subject  jsonb
) RETURNS boolean AS $$
    SELECT _record(
        'col_is_pk',
        actual IS NOT NULL AND actual = expected,
        description,
        CASE WHEN actual IS NOT NULL AND actual = expected THEN NULL
             ELSE subject || jsonb_build_object(
                 'have', to_jsonb(actual),
                 'want', to_jsonb(expected)
             ) END
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION col_is_pk(name, name, name[], text) RETURNS boolean AS $$
    SELECT _record_pk(_pk_columns($1, $2), $3, $4,
                      jsonb_build_object('schema', $1, 'table', $2));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION col_is_pk(name, name, name, text) RETURNS boolean AS $$
    SELECT col_is_pk($1, $2, ARRAY[$3]::name[], $4);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION col_is_pk(name, name[], text) RETURNS boolean AS $$
    SELECT _record_pk(_pk_columns($1), $2, $3, jsonb_build_object('table', $1));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION col_is_pk(name, name, text) RETURNS boolean AS $$
    SELECT col_is_pk($1, ARRAY[$2]::name[], $3);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_pk(name, name, text) RETURNS boolean AS $$
    SELECT _record(
        'has_pk',
        _pk_columns($1, $2) IS NOT NULL,
        $3,
        jsonb_build_object('schema', $1, 'table', $2)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_pk(name, text) RETURNS boolean AS $$
    SELECT _record(
        'has_pk',
        _pk_columns($1) IS NOT NULL,
        $2,
        jsonb_build_object('table', $1)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_pk(name) RETURNS boolean AS $$
    SELECT has_pk($1, 'table ' || quote_ident($1) || ' should have a primary key');
$$ LANGUAGE sql;

-- -- Schema-qualified convenience overloads ---------------------------------
--
-- pgTAP ships these, and a file that used one would abort with "function does
-- not exist" rather than recording a failed assertion -- the worst way to
-- break, because it stops the file instead of reporting.

CREATE OR REPLACE FUNCTION hasnt_table(name, name) RETURNS boolean AS $$
    SELECT hasnt_table($1, $2,
        'table ' || quote_ident($1) || '.' || quote_ident($2) || ' should not exist');
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_view(name, name, text) RETURNS boolean AS $$
    SELECT _record(
        'hasnt_view',
        NOT _relation_exists('{v,m}'::"char"[], $1, $2),
        $3,
        jsonb_build_object('schema', $1, 'view', $2)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_view(name, name) RETURNS boolean AS $$
    SELECT hasnt_view($1, $2,
        'view ' || quote_ident($1) || '.' || quote_ident($2) || ' should not exist');
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_sequence(name, name) RETURNS boolean AS $$
    SELECT has_sequence($1, $2,
        'sequence ' || quote_ident($1) || '.' || quote_ident($2) || ' should exist');
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_sequence(name, text) RETURNS boolean AS $$
    SELECT _record(
        'hasnt_sequence',
        NOT _relation_exists('{S}'::"char"[], $1),
        $2,
        jsonb_build_object('sequence', $1)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_sequence(name) RETURNS boolean AS $$
    SELECT hasnt_sequence($1, 'sequence ' || quote_ident($1) || ' should not exist');
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_column(name, name, name, text) RETURNS boolean AS $$
    SELECT _record(
        'hasnt_column',
        NOT _column_exists($1, $2, $3),
        $4,
        jsonb_build_object('schema', $1, 'table', $2, 'column', $3)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_column(name, name, name) RETURNS boolean AS $$
    SELECT hasnt_column($1, $2, $3,
        'column ' || quote_ident($1) || '.' || quote_ident($2) || '.'
                  || quote_ident($3) || ' should not exist');
$$ LANGUAGE sql;

-- The remaining col_is_pk shapes pgTAP ships.
--
-- Note that three bare literals resolve to `(name, name, text)` -- the
-- unqualified form with a description -- because `text` is preferred among
-- unknown literals. That is pgTAP's behaviour too, so the qualified
-- three-argument form is reached with explicit `::name` casts or by passing a
-- description as a fourth argument.
CREATE OR REPLACE FUNCTION col_is_pk(name, name, name) RETURNS boolean AS $$
    SELECT col_is_pk($1, $2, ARRAY[$3]::name[],
        'column ' || quote_ident($1) || '.' || quote_ident($2) || '.'
                  || quote_ident($3) || ' should be the primary key');
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION col_is_pk(name, name, name[]) RETURNS boolean AS $$
    SELECT col_is_pk($1, $2, $3,
        'table ' || quote_ident($1) || '.' || quote_ident($2)
        || ' should have primary key (' || array_to_string($3, ', ') || ')');
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION col_is_pk(name, name[]) RETURNS boolean AS $$
    SELECT col_is_pk($1, $2,
        'table ' || quote_ident($1) || ' should have primary key ('
        || array_to_string($2, ', ') || ')');
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_sequence(name, name, text) RETURNS boolean AS $$
    SELECT _record(
        'hasnt_sequence',
        NOT _relation_exists('{S}'::"char"[], $1, $2),
        $3,
        jsonb_build_object('schema', $1, 'sequence', $2)
    );
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_sequence(name, name) RETURNS boolean AS $$
    SELECT hasnt_sequence($1, $2,
        'sequence ' || quote_ident($1) || '.' || quote_ident($2) || ' should not exist');
$$ LANGUAGE sql;
