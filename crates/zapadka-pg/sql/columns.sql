-- Assertions about column properties.
--
-- PostgreSQL's catalogs are the authority here. A column is resolved to the
-- relation the session would use, then its properties are read once into a
-- JSON object. Keeping object resolution separate lets every assertion report
-- a missing relation or column instead of collapsing it into a false property.

CREATE OR REPLACE FUNCTION _column_relation_oid(
    schema_name name,
    relation_name name
) RETURNS oid AS $$
    SELECT c.oid
      FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = schema_name
       AND c.relname = relation_name
       AND c.relkind = ANY ('{r,p,v,m,f,c}'::"char"[]);
$$ LANGUAGE sql STABLE;

CREATE OR REPLACE FUNCTION _column_relation_oid(relation_name name)
RETURNS oid AS $$
    SELECT c.oid
      FROM pg_catalog.pg_class c
     WHERE c.relname = relation_name
       AND c.relkind = ANY ('{r,p,v,m,f,c}'::"char"[])
       AND pg_catalog.pg_table_is_visible(c.oid)
     LIMIT 1;
$$ LANGUAGE sql STABLE;

CREATE OR REPLACE FUNCTION _column_fact(relation_oid oid, column_name name)
RETURNS jsonb AS $$
    SELECT jsonb_build_object(
        'not_null', a.attnotnull,
        'has_default', a.atthasdef,
        'default_expression', pg_catalog.pg_get_expr(d.adbin, d.adrelid),
        'type', pg_catalog.format_type(a.atttypid, a.atttypmod)
    )
      FROM pg_catalog.pg_attribute a
      LEFT JOIN pg_catalog.pg_attrdef d
        ON d.adrelid = a.attrelid AND d.adnum = a.attnum
     WHERE a.attrelid = relation_oid
       AND a.attname = column_name
       AND a.attnum > 0
       AND NOT a.attisdropped;
$$ LANGUAGE sql STABLE;

CREATE OR REPLACE FUNCTION _column_subject(
    schema_name name,
    relation_name name,
    column_name name
) RETURNS jsonb AS $$
    SELECT jsonb_strip_nulls(jsonb_build_object(
        'schema', schema_name,
        'relation', relation_name,
        'column', column_name
    ));
$$ LANGUAGE sql IMMUTABLE;

CREATE OR REPLACE FUNCTION _record_column_property(
    assertion_kind text,
    relation_oid oid,
    fact jsonb,
    schema_name name,
    relation_name name,
    column_name name,
    property_name text,
    actual jsonb,
    expected jsonb,
    description text
) RETURNS boolean AS $$
DECLARE
    subject jsonb := _column_subject(schema_name, relation_name, column_name);
BEGIN
    IF relation_oid IS NULL THEN
        RETURN _record(assertion_kind, false, description, subject || jsonb_build_object(
            'kind', 'missing_object',
            'object_type', 'relation'
        ));
    END IF;

    IF fact IS NULL THEN
        RETURN _record(assertion_kind, false, description, subject || jsonb_build_object(
            'kind', 'missing_object',
            'object_type', 'column'
        ));
    END IF;

    RETURN _record(assertion_kind, actual = expected, description,
        CASE WHEN actual = expected THEN NULL
             ELSE subject || jsonb_build_object(
                 'kind', 'column_property',
                 'property', property_name,
                 'have', actual,
                 'want', expected
             ) END);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION _record_column_nullability(
    assertion_kind text,
    relation_oid oid,
    schema_name name,
    relation_name name,
    column_name name,
    expected_not_null boolean,
    description text
) RETURNS boolean AS $$
DECLARE
    fact jsonb := _column_fact(relation_oid, column_name);
BEGIN
    RETURN _record_column_property(
        assertion_kind, relation_oid, fact, schema_name, relation_name, column_name,
        'not_null', fact -> 'not_null', to_jsonb(expected_not_null), description
    );
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION col_not_null(name, name, name, text)
RETURNS boolean AS $$
    SELECT _record_column_nullability(
        'col_not_null', _column_relation_oid($1, $2), $1, $2, $3, true, $4
    );
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_not_null(name, name, name, text) IS
    'Assert that a qualified column is NOT NULL.';

CREATE OR REPLACE FUNCTION col_not_null(name, name, name)
RETURNS boolean AS $$
    SELECT col_not_null($1, $2, $3,
        'column ' || quote_ident($1) || '.' || quote_ident($2) || '.'
        || quote_ident($3) || ' should be NOT NULL');
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_not_null(name, name, name) IS
    'Assert that a qualified column is NOT NULL.';

CREATE OR REPLACE FUNCTION col_not_null(name, name, text)
RETURNS boolean AS $$
    SELECT _record_column_nullability(
        'col_not_null', _column_relation_oid($1), NULL, $1, $2, true, $3
    );
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_not_null(name, name, text) IS
    'Assert that a visible column is NOT NULL.';

CREATE OR REPLACE FUNCTION col_not_null(name, name)
RETURNS boolean AS $$
    SELECT col_not_null($1, $2,
        'column ' || quote_ident($1) || '.' || quote_ident($2) || ' should be NOT NULL');
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_not_null(name, name) IS
    'Assert that a visible column is NOT NULL.';

CREATE OR REPLACE FUNCTION col_not_null_in(name, name, name, text)
RETURNS boolean AS $$
    SELECT col_not_null($1::name, $2::name, $3::name, $4);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_not_null_in(name, name, name, text) IS
    'Unambiguously assert that a qualified column is NOT NULL.';

CREATE OR REPLACE FUNCTION col_not_null_in(name, name, name)
RETURNS boolean AS $$
    SELECT col_not_null($1::name, $2::name, $3::name);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_not_null_in(name, name, name) IS
    'Unambiguously assert that a qualified column is NOT NULL.';

CREATE OR REPLACE FUNCTION col_is_null(name, name, name, text)
RETURNS boolean AS $$
    SELECT _record_column_nullability(
        'col_is_null', _column_relation_oid($1, $2), $1, $2, $3, false, $4
    );
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_is_null(name, name, name, text) IS
    'Assert that a qualified column allows NULL.';

CREATE OR REPLACE FUNCTION col_is_null(name, name, name)
RETURNS boolean AS $$
    SELECT col_is_null($1, $2, $3,
        'column ' || quote_ident($1) || '.' || quote_ident($2) || '.'
        || quote_ident($3) || ' should allow NULL');
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_is_null(name, name, name) IS
    'Assert that a qualified column allows NULL.';

CREATE OR REPLACE FUNCTION col_is_null(name, name, text)
RETURNS boolean AS $$
    SELECT _record_column_nullability(
        'col_is_null', _column_relation_oid($1), NULL, $1, $2, false, $3
    );
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_is_null(name, name, text) IS
    'Assert that a visible column allows NULL.';

CREATE OR REPLACE FUNCTION col_is_null(name, name)
RETURNS boolean AS $$
    SELECT col_is_null($1, $2,
        'column ' || quote_ident($1) || '.' || quote_ident($2) || ' should allow NULL');
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_is_null(name, name) IS
    'Assert that a visible column allows NULL.';

CREATE OR REPLACE FUNCTION col_is_null_in(name, name, name, text)
RETURNS boolean AS $$
    SELECT col_is_null($1::name, $2::name, $3::name, $4);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_is_null_in(name, name, name, text) IS
    'Unambiguously assert that a qualified column allows NULL.';

CREATE OR REPLACE FUNCTION col_is_null_in(name, name, name)
RETURNS boolean AS $$
    SELECT col_is_null($1::name, $2::name, $3::name);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_is_null_in(name, name, name) IS
    'Unambiguously assert that a qualified column allows NULL.';

CREATE OR REPLACE FUNCTION _record_column_default_presence(
    assertion_kind text,
    relation_oid oid,
    schema_name name,
    relation_name name,
    column_name name,
    expected_has_default boolean,
    description text
) RETURNS boolean AS $$
DECLARE
    fact jsonb := _column_fact(relation_oid, column_name);
BEGIN
    RETURN _record_column_property(
        assertion_kind, relation_oid, fact, schema_name, relation_name, column_name,
        'has_default', fact -> 'has_default', to_jsonb(expected_has_default), description
    );
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION col_has_default(name, name, name, text)
RETURNS boolean AS $$
    SELECT _record_column_default_presence(
        'col_has_default', _column_relation_oid($1, $2), $1, $2, $3, true, $4
    );
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_has_default(name, name, name, text) IS
    'Assert that a qualified column has a default.';

CREATE OR REPLACE FUNCTION col_has_default(name, name, name)
RETURNS boolean AS $$
    SELECT col_has_default($1, $2, $3,
        'column ' || quote_ident($1) || '.' || quote_ident($2) || '.'
        || quote_ident($3) || ' should have a default');
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_has_default(name, name, name) IS
    'Assert that a qualified column has a default.';

CREATE OR REPLACE FUNCTION col_has_default(name, name, text)
RETURNS boolean AS $$
    SELECT _record_column_default_presence(
        'col_has_default', _column_relation_oid($1), NULL, $1, $2, true, $3
    );
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_has_default(name, name, text) IS
    'Assert that a visible column has a default.';

CREATE OR REPLACE FUNCTION col_has_default(name, name)
RETURNS boolean AS $$
    SELECT col_has_default($1, $2,
        'column ' || quote_ident($1) || '.' || quote_ident($2) || ' should have a default');
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_has_default(name, name) IS
    'Assert that a visible column has a default.';

CREATE OR REPLACE FUNCTION col_has_default_in(name, name, name, text)
RETURNS boolean AS $$
    SELECT col_has_default($1::name, $2::name, $3::name, $4);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_has_default_in(name, name, name, text) IS
    'Unambiguously assert that a qualified column has a default.';

CREATE OR REPLACE FUNCTION col_has_default_in(name, name, name)
RETURNS boolean AS $$
    SELECT col_has_default($1::name, $2::name, $3::name);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_has_default_in(name, name, name) IS
    'Unambiguously assert that a qualified column has a default.';

CREATE OR REPLACE FUNCTION col_hasnt_default(name, name, name, text)
RETURNS boolean AS $$
    SELECT _record_column_default_presence(
        'col_hasnt_default', _column_relation_oid($1, $2), $1, $2, $3, false, $4
    );
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_hasnt_default(name, name, name, text) IS
    'Assert that a qualified column has no default.';

CREATE OR REPLACE FUNCTION col_hasnt_default(name, name, name)
RETURNS boolean AS $$
    SELECT col_hasnt_default($1, $2, $3,
        'column ' || quote_ident($1) || '.' || quote_ident($2) || '.'
        || quote_ident($3) || ' should not have a default');
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_hasnt_default(name, name, name) IS
    'Assert that a qualified column has no default.';

CREATE OR REPLACE FUNCTION col_hasnt_default(name, name, text)
RETURNS boolean AS $$
    SELECT _record_column_default_presence(
        'col_hasnt_default', _column_relation_oid($1), NULL, $1, $2, false, $3
    );
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_hasnt_default(name, name, text) IS
    'Assert that a visible column has no default.';

CREATE OR REPLACE FUNCTION col_hasnt_default(name, name)
RETURNS boolean AS $$
    SELECT col_hasnt_default($1, $2,
        'column ' || quote_ident($1) || '.' || quote_ident($2)
        || ' should not have a default');
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_hasnt_default(name, name) IS
    'Assert that a visible column has no default.';

CREATE OR REPLACE FUNCTION col_hasnt_default_in(name, name, name, text)
RETURNS boolean AS $$
    SELECT col_hasnt_default($1::name, $2::name, $3::name, $4);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_hasnt_default_in(name, name, name, text) IS
    'Unambiguously assert that a qualified column has no default.';

CREATE OR REPLACE FUNCTION col_hasnt_default_in(name, name, name)
RETURNS boolean AS $$
    SELECT col_hasnt_default($1::name, $2::name, $3::name);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_hasnt_default_in(name, name, name) IS
    'Unambiguously assert that a qualified column has no default.';

CREATE OR REPLACE FUNCTION _normalise_type(type_name text)
RETURNS text AS $$
BEGIN
    RETURN pg_catalog.format_type(
        pg_catalog.to_regtype(type_name),
        pg_catalog.to_regtypemod(type_name)
    );
EXCEPTION WHEN OTHERS THEN
    RETURN NULL;
END;
$$ LANGUAGE plpgsql STABLE;

CREATE OR REPLACE FUNCTION _record_column_type(
    relation_oid oid,
    schema_name name,
    relation_name name,
    column_name name,
    expected_type text,
    description text
) RETURNS boolean AS $$
DECLARE
    fact jsonb := _column_fact(relation_oid, column_name);
    normalised text;
    subject jsonb := _column_subject(schema_name, relation_name, column_name);
BEGIN
    IF relation_oid IS NULL OR fact IS NULL THEN
        RETURN _record_column_property(
            'col_type_is', relation_oid, fact, schema_name, relation_name, column_name,
            'type', fact -> 'type', to_jsonb(expected_type), description
        );
    END IF;

    normalised := _normalise_type(expected_type);
    IF normalised IS NULL THEN
        RETURN _record('col_type_is', false, description, subject || jsonb_build_object(
            'kind', 'invalid_expected',
            'property', 'type',
            'expected', expected_type
        ));
    END IF;

    RETURN _record_column_property(
        'col_type_is', relation_oid, fact, schema_name, relation_name, column_name,
        'type', fact -> 'type', to_jsonb(normalised), description
    );
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION col_type_is(name, name, name, text, text)
RETURNS boolean AS $$
    SELECT _record_column_type(_column_relation_oid($1, $2), $1, $2, $3, $4, $5);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_type_is(name, name, name, text, text) IS
    'Assert the PostgreSQL type of a qualified column.';

CREATE OR REPLACE FUNCTION col_type_is(name, name, name, text)
RETURNS boolean AS $$
    SELECT col_type_is($1, $2, $3, $4,
        'column ' || quote_ident($1) || '.' || quote_ident($2) || '.'
        || quote_ident($3) || ' should have type ' || $4);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_type_is(name, name, name, text) IS
    'Assert the PostgreSQL type of a qualified column.';

CREATE OR REPLACE FUNCTION col_type_is(name, name, text, text)
RETURNS boolean AS $$
    SELECT _record_column_type(_column_relation_oid($1), NULL, $1, $2, $3, $4);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_type_is(name, name, text, text) IS
    'Assert the PostgreSQL type of a visible column.';

CREATE OR REPLACE FUNCTION col_type_is(name, name, text)
RETURNS boolean AS $$
    SELECT col_type_is($1, $2, $3,
        'column ' || quote_ident($1) || '.' || quote_ident($2)
        || ' should have type ' || $3);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_type_is(name, name, text) IS
    'Assert the PostgreSQL type of a visible column.';

CREATE OR REPLACE FUNCTION col_type_is_in(name, name, name, text, text)
RETURNS boolean AS $$
    SELECT col_type_is($1::name, $2::name, $3::name, $4, $5);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_type_is_in(name, name, name, text, text) IS
    'Unambiguously assert the PostgreSQL type of a qualified column.';

CREATE OR REPLACE FUNCTION col_type_is_in(name, name, name, text)
RETURNS boolean AS $$
    SELECT col_type_is($1::name, $2::name, $3::name, $4);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_type_is_in(name, name, name, text) IS
    'Unambiguously assert the PostgreSQL type of a qualified column.';

CREATE OR REPLACE FUNCTION _record_column_default(
    relation_oid oid,
    schema_name name,
    relation_name name,
    column_name name,
    expected anyelement,
    description text
) RETURNS boolean AS $$
DECLARE
    fact jsonb := _column_fact(relation_oid, column_name);
    subject jsonb := _column_subject(schema_name, relation_name, column_name);
    expression text;
    column_type text;
    actual_json jsonb;
    actual_display text;
    matches boolean;
BEGIN
    IF relation_oid IS NULL OR fact IS NULL THEN
        RETURN _record_column_property(
            'col_default_is', relation_oid, fact, schema_name, relation_name, column_name,
            'default', fact -> 'default_expression', to_jsonb(expected), description
        );
    END IF;

    IF NOT (fact ->> 'has_default')::boolean THEN
        RETURN _record('col_default_is', false, description, subject || jsonb_build_object(
            'kind', 'column_property',
            'property', 'default',
            'problem', 'column_has_no_default',
            'want', _describe(expected)
        ));
    END IF;

    expression := fact ->> 'default_expression';
    column_type := fact ->> 'type';
    BEGIN
        EXECUTE pg_catalog.format(
            'SELECT to_jsonb(value), value::text, value IS NOT DISTINCT FROM ($1::%s) '
            'FROM (SELECT (%s) AS value) actual',
            column_type,
            expression
        ) INTO actual_json, actual_display, matches USING expected;
    EXCEPTION WHEN OTHERS THEN
        RETURN _record('col_default_is', false, description, subject || jsonb_build_object(
            'kind', 'invalid_expected',
            'property', 'default',
            'expression', expression,
            'column_type', column_type,
            'expected', _describe(expected),
            'sqlstate', SQLSTATE,
            'message', SQLERRM
        ));
    END;

    RETURN _record('col_default_is', matches, description,
        CASE WHEN matches THEN NULL
             ELSE subject || jsonb_build_object(
                 'kind', 'column_property',
                 'property', 'default',
                 'expression', expression,
                 'column_type', column_type,
                 'have', jsonb_build_object(
                     'json', coalesce(actual_json, 'null'::jsonb),
                     'display', actual_display,
                     'is_null', actual_json IS NULL
                 ),
                 'want', _describe(expected)
             ) END);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION col_default_is(name, name, name, anyelement, text)
RETURNS boolean AS $$
    SELECT _record_column_default(_column_relation_oid($1, $2), $1, $2, $3, $4, $5);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_default_is(name, name, name, anyelement, text) IS
    'Assert the value of a qualified column default.';

CREATE OR REPLACE FUNCTION col_default_is(name, name, name, text, text)
RETURNS boolean AS $$
    SELECT _record_column_default(_column_relation_oid($1, $2), $1, $2, $3, $4, $5);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_default_is(name, name, name, text, text) IS
    'Assert the value of a qualified column default supplied as text.';

CREATE OR REPLACE FUNCTION col_default_is(name, name, anyelement, text)
RETURNS boolean AS $$
    SELECT _record_column_default(_column_relation_oid($1), NULL, $1, $2, $3, $4);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_default_is(name, name, anyelement, text) IS
    'Assert the value of a visible column default.';

CREATE OR REPLACE FUNCTION col_default_is(name, name, text, text)
RETURNS boolean AS $$
    SELECT _record_column_default(_column_relation_oid($1), NULL, $1, $2, $3, $4);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_default_is(name, name, text, text) IS
    'Assert the value of a visible column default supplied as text.';

CREATE OR REPLACE FUNCTION col_default_is_in(name, name, name, anyelement, text)
RETURNS boolean AS $$
    SELECT col_default_is($1::name, $2::name, $3::name, $4, $5);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_default_is_in(name, name, name, anyelement, text) IS
    'Unambiguously assert the value of a qualified column default.';

CREATE OR REPLACE FUNCTION col_default_is_in(name, name, name, text, text)
RETURNS boolean AS $$
    SELECT col_default_is($1::name, $2::name, $3::name, $4, $5);
$$ LANGUAGE sql;
COMMENT ON FUNCTION col_default_is_in(name, name, name, text, text) IS
    'Unambiguously assert the value of a qualified column default supplied as text.';
