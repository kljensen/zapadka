-- Assertions about privileges, ownership, and the type system.
--
-- Privilege tests are the ones people most often get wrong and most need good
-- output from, because the answer is a *set* and the interesting part is which
-- element is missing. pgTAP renders the two sets as sentences; here the
-- difference is reported as two lists, so a reader is told "DELETE was granted
-- and should not have been" rather than being handed two strings to compare.

-- -- Privileges ---------------------------------------------------------------

-- Every table privilege PostgreSQL 18 defines.
--
-- `MAINTAIN` is new in 18 and covers VACUUM, ANALYZE, REINDEX and friends. A
-- library that only knew the older list would silently omit it from every
-- comparison, and an unexpected grant would go unreported.
CREATE OR REPLACE FUNCTION _table_privilege_names() RETURNS text[] AS $$
    SELECT ARRAY[
        'SELECT', 'INSERT', 'UPDATE', 'DELETE',
        'TRUNCATE', 'REFERENCES', 'TRIGGER', 'MAINTAIN'
    ];
$$ LANGUAGE sql IMMUTABLE;

-- The privileges a role effectively holds on a relation.
--
-- "Effectively": `has_table_privilege` accounts for grants to PUBLIC and for
-- role membership, which is what a test usually means by "can this role read
-- the table". A direct-grant-only answer would pass while the role could still
-- read everything through PUBLIC.
CREATE OR REPLACE FUNCTION _table_privs(
    schema_name name,
    table_name  name,
    role_name   name
) RETURNS text[] AS $$
    SELECT coalesce(array_agg(p ORDER BY p), ARRAY[]::text[])
      FROM unnest(_table_privilege_names()) AS p
     WHERE pg_catalog.has_table_privilege(
               role_name,
               (quote_ident(schema_name) || '.' || quote_ident(table_name))::regclass,
               p
           );
$$ LANGUAGE sql STABLE;

-- Records a privilege-set comparison.
CREATE OR REPLACE FUNCTION _record_privs(
    kind        text,
    actual      text[],
    expected    text[],
    description text,
    subject     jsonb
) RETURNS boolean AS $$
DECLARE
    want    text[] := coalesce((SELECT array_agg(p ORDER BY p) FROM unnest(expected) p),
                               ARRAY[]::text[]);
    missing text[];
    extra   text[];
BEGIN
    SELECT coalesce(array_agg(p ORDER BY p), ARRAY[]::text[]) INTO missing
      FROM unnest(want) p WHERE p <> ALL (actual);
    SELECT coalesce(array_agg(p ORDER BY p), ARRAY[]::text[]) INTO extra
      FROM unnest(actual) p WHERE p <> ALL (want);

    RETURN _record(kind, actual = want, description,
        CASE WHEN actual = want THEN NULL
             ELSE subject || jsonb_build_object(
                 'kind', 'privileges',
                 'have', to_jsonb(actual),
                 'want', to_jsonb(want),
                 'missing', to_jsonb(missing),
                 'unexpected', to_jsonb(extra)
             ) END);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION table_privs_are(name, name, name, text[], text)
RETURNS boolean AS $$
    SELECT _record_privs('table_privs_are', _table_privs($1, $2, $3), $4, $5,
        jsonb_build_object('schema', $1, 'table', $2, 'role', $3));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION table_privs_are(name, name, name, text[])
RETURNS boolean AS $$
    SELECT table_privs_are($1, $2, $3, $4,
        'role ' || quote_ident($3) || ' should be granted '
        || coalesce(nullif(array_to_string($4, ', '), ''), 'nothing')
        || ' on ' || quote_ident($1) || '.' || quote_ident($2));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION table_privs_are(name, name, text[], text)
RETURNS boolean AS $$
    SELECT _record_privs('table_privs_are',
        _table_privs((SELECT n.nspname
                        FROM pg_catalog.pg_class c
                        JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                       WHERE c.relname = $1
                         AND pg_catalog.pg_table_is_visible(c.oid)
                       LIMIT 1),
                     $1, $2),
        $3, $4, jsonb_build_object('table', $1, 'role', $2));
$$ LANGUAGE sql;

-- Schema privileges. A different set of names entirely: you cannot SELECT from
-- a schema.
CREATE OR REPLACE FUNCTION _schema_privs(schema_name name, role_name name)
RETURNS text[] AS $$
    SELECT coalesce(array_agg(p ORDER BY p), ARRAY[]::text[])
      FROM unnest(ARRAY['CREATE', 'USAGE']) AS p
     WHERE pg_catalog.has_schema_privilege(role_name, schema_name, p);
$$ LANGUAGE sql STABLE;

CREATE OR REPLACE FUNCTION schema_privs_are(name, name, text[], text)
RETURNS boolean AS $$
    SELECT _record_privs('schema_privs_are', _schema_privs($1, $2), $3, $4,
        jsonb_build_object('schema', $1, 'role', $2));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION schema_privs_are(name, name, text[])
RETURNS boolean AS $$
    SELECT schema_privs_are($1, $2, $3,
        'role ' || quote_ident($2) || ' should be granted '
        || coalesce(nullif(array_to_string($3, ', '), ''), 'nothing')
        || ' on schema ' || quote_ident($1));
$$ LANGUAGE sql;

-- Function privileges. EXECUTE is the only one there is, so this is really a
-- boolean wearing a set's clothing -- kept as a set for symmetry with the
-- others, and because that is the shape pgTAP users expect.
CREATE OR REPLACE FUNCTION _function_privs(
    schema_name name,
    function_name name,
    argument_types text[],
    role_name name
) RETURNS text[] AS $$
    SELECT coalesce(array_agg(p ORDER BY p), ARRAY[]::text[])
      FROM unnest(ARRAY['EXECUTE']) AS p
     WHERE pg_catalog.has_function_privilege(
               role_name,
               (quote_ident(schema_name) || '.' || quote_ident(function_name)
                || '(' || array_to_string(argument_types, ',') || ')')::regprocedure,
               p
           );
$$ LANGUAGE sql STABLE;

CREATE OR REPLACE FUNCTION function_privs_are(name, name, text[], name, text[], text)
RETURNS boolean AS $$
    SELECT _record_privs('function_privs_are',
        _function_privs($1, $2, $3, $4), $5, $6,
        jsonb_build_object('schema', $1, 'function', $2,
                           'arguments', to_jsonb($3), 'role', $4));
$$ LANGUAGE sql;

-- -- Ownership ----------------------------------------------------------------

CREATE OR REPLACE FUNCTION _relation_owner(kinds "char"[], schema_name name, relation_name name)
RETURNS name AS $$
    SELECT r.rolname
      FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
      JOIN pg_catalog.pg_roles r ON r.oid = c.relowner
     WHERE c.relname = relation_name
       AND n.nspname = schema_name
       AND c.relkind = ANY(kinds);
$$ LANGUAGE sql STABLE;

CREATE OR REPLACE FUNCTION _record_owner(
    kind text, actual name, expected name, description text, subject jsonb
) RETURNS boolean AS $$
    SELECT _record(kind, actual IS NOT NULL AND actual = expected, description,
        CASE WHEN actual IS NOT NULL AND actual = expected THEN NULL
             ELSE subject || jsonb_build_object(
                 'kind', 'ownership',
                 'have', actual,
                 'want', expected
             ) END);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION table_owner_is(name, name, name, text) RETURNS boolean AS $$
    SELECT _record_owner('table_owner_is', _relation_owner('{r,p}'::"char"[], $1, $2), $3, $4,
        jsonb_build_object('schema', $1, 'table', $2));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION table_owner_is(name, name, name) RETURNS boolean AS $$
    SELECT table_owner_is($1, $2, $3,
        'table ' || quote_ident($1) || '.' || quote_ident($2)
        || ' should be owned by ' || quote_ident($3));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION view_owner_is(name, name, name, text) RETURNS boolean AS $$
    SELECT _record_owner('view_owner_is', _relation_owner('{v,m}'::"char"[], $1, $2), $3, $4,
        jsonb_build_object('schema', $1, 'view', $2));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION view_owner_is(name, name, name) RETURNS boolean AS $$
    SELECT view_owner_is($1, $2, $3,
        'view ' || quote_ident($1) || '.' || quote_ident($2)
        || ' should be owned by ' || quote_ident($3));
$$ LANGUAGE sql;

-- -- Enums ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION _type_exists(kind "char", schema_name name, type_name name)
RETURNS boolean AS $$
    SELECT EXISTS (
        SELECT 1
          FROM pg_catalog.pg_type t
          JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
         WHERE t.typname = type_name
           AND n.nspname = schema_name
           AND t.typtype = kind
    );
$$ LANGUAGE sql STABLE;

CREATE OR REPLACE FUNCTION _type_exists(kind "char", type_name name)
RETURNS boolean AS $$
    SELECT EXISTS (
        SELECT 1
          FROM pg_catalog.pg_type t
         WHERE t.typname = type_name
           AND t.typtype = kind
           AND pg_catalog.pg_type_is_visible(t.oid)
    );
$$ LANGUAGE sql STABLE;

CREATE OR REPLACE FUNCTION has_enum(name, name, text) RETURNS boolean AS $$
    SELECT _record('has_enum', _type_exists('e'::"char", $1, $2), $3,
        jsonb_build_object('schema', $1, 'enum', $2));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_enum(name, text) RETURNS boolean AS $$
    SELECT _record('has_enum', _type_exists('e'::"char", $1), $2,
        jsonb_build_object('enum', $1));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_enum(name) RETURNS boolean AS $$
    SELECT has_enum($1, 'enum ' || quote_ident($1) || ' should exist');
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_enum(name, text) RETURNS boolean AS $$
    SELECT _record('hasnt_enum', NOT _type_exists('e'::"char", $1), $2,
        jsonb_build_object('enum', $1));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_enum(name) RETURNS boolean AS $$
    SELECT hasnt_enum($1, 'enum ' || quote_ident($1) || ' should not exist');
$$ LANGUAGE sql;

-- Enum labels in declaration order.
--
-- Order matters: it decides how the type sorts, so a test that ignored it would
-- pass on an enum whose ordering had silently changed.
CREATE OR REPLACE FUNCTION _enum_labels(schema_name name, type_name name)
RETURNS text[] AS $$
    SELECT array_agg(e.enumlabel::text ORDER BY e.enumsortorder)
      FROM pg_catalog.pg_enum e
      JOIN pg_catalog.pg_type t ON t.oid = e.enumtypid
      JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
     WHERE t.typname = type_name AND n.nspname = schema_name;
$$ LANGUAGE sql STABLE;

CREATE OR REPLACE FUNCTION _enum_labels(type_name name) RETURNS text[] AS $$
    SELECT array_agg(e.enumlabel::text ORDER BY e.enumsortorder)
      FROM pg_catalog.pg_enum e
      JOIN pg_catalog.pg_type t ON t.oid = e.enumtypid
     WHERE t.typname = type_name
       AND pg_catalog.pg_type_is_visible(t.oid);
$$ LANGUAGE sql STABLE;

CREATE OR REPLACE FUNCTION enum_has_labels(name, name, text[], text) RETURNS boolean AS $$
    SELECT _record('enum_has_labels', _enum_labels($1, $2) = $3, $4,
        CASE WHEN _enum_labels($1, $2) = $3 THEN NULL ELSE jsonb_build_object(
            'kind', 'enum_labels', 'schema', $1, 'enum', $2,
            'have', to_jsonb(_enum_labels($1, $2)), 'want', to_jsonb($3)
        ) END);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION enum_has_labels(name, text[], text) RETURNS boolean AS $$
    SELECT _record('enum_has_labels', _enum_labels($1) = $2, $3,
        CASE WHEN _enum_labels($1) = $2 THEN NULL ELSE jsonb_build_object(
            'kind', 'enum_labels', 'enum', $1,
            'have', to_jsonb(_enum_labels($1)), 'want', to_jsonb($2)
        ) END);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION enum_has_labels(name, text[]) RETURNS boolean AS $$
    SELECT enum_has_labels($1, $2, 'enum ' || quote_ident($1) || ' should have the expected labels');
$$ LANGUAGE sql;

-- -- Domains -------------------------------------------------------------------

CREATE OR REPLACE FUNCTION has_domain(name, name, text) RETURNS boolean AS $$
    SELECT _record('has_domain', _type_exists('d'::"char", $1, $2), $3,
        jsonb_build_object('schema', $1, 'domain', $2));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_domain(name, text) RETURNS boolean AS $$
    SELECT _record('has_domain', _type_exists('d'::"char", $1), $2,
        jsonb_build_object('domain', $1));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_domain(name) RETURNS boolean AS $$
    SELECT has_domain($1, 'domain ' || quote_ident($1) || ' should exist');
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_domain(name, text) RETURNS boolean AS $$
    SELECT _record('hasnt_domain', NOT _type_exists('d'::"char", $1), $2,
        jsonb_build_object('domain', $1));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_domain(name) RETURNS boolean AS $$
    SELECT hasnt_domain($1, 'domain ' || quote_ident($1) || ' should not exist');
$$ LANGUAGE sql;

-- The type a domain is built on.
CREATE OR REPLACE FUNCTION _domain_base(schema_name name, type_name name) RETURNS text AS $$
    SELECT pg_catalog.format_type(t.typbasetype, t.typtypmod)
      FROM pg_catalog.pg_type t
      JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace
     WHERE t.typname = type_name AND n.nspname = schema_name AND t.typtype = 'd';
$$ LANGUAGE sql STABLE;

CREATE OR REPLACE FUNCTION domain_type_is(name, name, text, text) RETURNS boolean AS $$
    SELECT _record('domain_type_is', _domain_base($1, $2) = $3, $4,
        CASE WHEN _domain_base($1, $2) = $3 THEN NULL ELSE jsonb_build_object(
            'kind', 'domain_type', 'schema', $1, 'domain', $2,
            'have', _domain_base($1, $2), 'want', $3
        ) END);
$$ LANGUAGE sql;

-- -- Casts ---------------------------------------------------------------------

CREATE OR REPLACE FUNCTION _cast_context(source_type text, target_type text) RETURNS "char" AS $$
    SELECT c.castcontext
      FROM pg_catalog.pg_cast c
     WHERE c.castsource = source_type::regtype
       AND c.casttarget = target_type::regtype;
$$ LANGUAGE sql STABLE;

CREATE OR REPLACE FUNCTION has_cast(text, text, text) RETURNS boolean AS $$
    SELECT _record('has_cast', _cast_context($1, $2) IS NOT NULL, $3,
        jsonb_build_object('source', $1, 'target', $2));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_cast(text, text) RETURNS boolean AS $$
    SELECT has_cast($1, $2, 'a cast from ' || $1 || ' to ' || $2 || ' should exist');
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION hasnt_cast(text, text, text) RETURNS boolean AS $$
    SELECT _record('hasnt_cast', _cast_context($1, $2) IS NULL, $3,
        jsonb_build_object('source', $1, 'target', $2));
$$ LANGUAGE sql;

-- `cast_context_is(source, target, context, description)` where context is
-- 'implicit', 'assignment' or 'explicit'.
--
-- Worth testing: an implicit cast changes overload resolution everywhere, which
-- is exactly the kind of change that breaks queries far from where it was made.
CREATE OR REPLACE FUNCTION cast_context_is(text, text, text, text) RETURNS boolean AS $$
DECLARE
    actual text := CASE _cast_context($1, $2)
                       WHEN 'i' THEN 'implicit'
                       WHEN 'a' THEN 'assignment'
                       WHEN 'e' THEN 'explicit'
                       ELSE NULL
                   END;
BEGIN
    RETURN _record('cast_context_is', actual IS NOT NULL AND actual = lower($3), $4,
        CASE WHEN actual IS NOT NULL AND actual = lower($3) THEN NULL
             ELSE jsonb_build_object(
                 'kind', 'cast_context', 'source', $1, 'target', $2,
                 'have', actual, 'want', lower($3)
             ) END);
END;
$$ LANGUAGE plpgsql;

CREATE OR REPLACE FUNCTION cast_context_is(text, text, text) RETURNS boolean AS $$
    SELECT cast_context_is($1, $2, $3,
        'the cast from ' || $1 || ' to ' || $2 || ' should be ' || lower($3));
$$ LANGUAGE sql;

-- -- Operators -------------------------------------------------------------------

-- Whether an operator with the given operand and result types exists.
--
-- NULL for `left_type` or `right_type` means the operator is unary on that
-- side, which is how `pg_operator` records prefix operators.
CREATE OR REPLACE FUNCTION _operator_exists(
    left_type   text,
    operator    name,
    right_type  text,
    result_type text
) RETURNS boolean AS $$
    SELECT EXISTS (
        SELECT 1
          FROM pg_catalog.pg_operator o
         WHERE o.oprname = operator
           AND (left_type IS NULL AND o.oprleft = 0
                OR o.oprleft = left_type::regtype)
           AND (right_type IS NULL AND o.oprright = 0
                OR o.oprright = right_type::regtype)
           AND (result_type IS NULL OR o.oprresult = result_type::regtype)
    );
$$ LANGUAGE sql STABLE;

CREATE OR REPLACE FUNCTION has_operator(text, name, text, text, text) RETURNS boolean AS $$
    SELECT _record('has_operator', _operator_exists($1, $2, $3, $4), $5,
        jsonb_build_object('left', $1, 'operator', $2, 'right', $3, 'returns', $4));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_operator(text, name, text, text) RETURNS boolean AS $$
    SELECT has_operator($1, $2, $3, $4,
        'operator ' || $1 || ' ' || $2 || ' ' || $3 || ' should return ' || $4);
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_operator(text, name, text) RETURNS boolean AS $$
    SELECT _record('has_operator', _operator_exists($1, $2, $3, NULL), NULL,
        jsonb_build_object('left', $1, 'operator', $2, 'right', $3));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_leftop(name, text, text, text) RETURNS boolean AS $$
    SELECT _record('has_leftop', _operator_exists(NULL, $1, $2, $3), $4,
        jsonb_build_object('operator', $1, 'right', $2, 'returns', $3));
$$ LANGUAGE sql;

CREATE OR REPLACE FUNCTION has_rightop(text, name, text, text) RETURNS boolean AS $$
    SELECT _record('has_rightop', _operator_exists($1, $2, NULL, $3), $4,
        jsonb_build_object('left', $1, 'operator', $2, 'returns', $3));
$$ LANGUAGE sql;
