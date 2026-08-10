//! Translates a `libpg_query` JSON parse tree into Zapadka's statement
//! vocabulary.
//!
//! The vocabulary is deliberately narrow. It carries only the facts Zapadka acts
//! on: whether a statement takes over the transaction boundary, and the
//! structural properties `lint` warns about. Anything else collapses into
//! [`StatementKind::Other`], so unfamiliar SQL is never rejected merely because
//! this module has not learned about it.

use serde_json::Value;

use crate::ParsedScript;

/// A top-level statement in a parsed script.
#[derive(Debug, Clone)]
pub struct Statement {
    /// The upstream parse-tree node name, e.g. `CreateStmt`. Carried for
    /// diagnostics only; Zapadka never branches on it outside this crate.
    pub node_type: String,
    /// What this statement means to Zapadka.
    pub kind: StatementKind,
    /// 0-based byte offset of the statement within the script.
    pub location: usize,
    /// Byte length of the statement, when upstream reported one.
    pub length: Option<usize>,
    /// 1-based line of [`Statement::location`] within the script.
    pub line: usize,
}

impl Statement {
    /// Returns the statement's text, given the script it came from.
    pub fn text<'a>(&self, script: &'a str) -> &'a str {
        let start = self.location.min(script.len());
        let end = match self.length {
            Some(length) => (start + length).min(script.len()),
            None => script.len(),
        };
        script[start..end].trim()
    }
}

/// The facts Zapadka acts on for a single statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementKind {
    /// Takes the transaction boundary away from the runner. Always rejected.
    TransactionControl(TransactionOperation),
    /// Builds an index. Without `CONCURRENTLY` this blocks writes to the table
    /// for the duration of the build.
    CreateIndex {
        /// The table being indexed, when the statement named one.
        relation: Option<QualifiedName>,
        /// Whether `CONCURRENTLY` was given, which PostgreSQL forbids inside a
        /// transaction block.
        concurrent: bool,
        /// Whether the index enforces uniqueness.
        unique: bool,
    },
    /// Creates a table, which by definition has no existing rows to lock.
    CreateTable {
        /// The table being created.
        relation: Option<QualifiedName>,
    },
    /// Alters a table. The risk lives in the individual actions.
    AlterTable {
        /// The table being altered.
        relation: Option<QualifiedName>,
        /// The actions applied, in the order they were written.
        actions: Vec<AlterTableAction>,
    },
    /// Removes an object.
    Drop {
        /// What kind of object is being removed.
        object: DropObject,
        /// Whether `CASCADE` extends the drop to dependent objects.
        cascade: bool,
        /// Whether `CONCURRENTLY` was given, which applies to indexes and which
        /// PostgreSQL forbids inside a transaction block.
        concurrent: bool,
    },
    /// Removes every row from a table.
    Truncate {
        /// Whether `CASCADE` extends the truncation to referencing tables.
        cascade: bool,
    },
    /// Renames an object, which breaks application code still using the old
    /// name.
    Rename {
        /// What kind of object is being renamed.
        object: DropObject,
        /// The new name.
        to: String,
    },
    /// Reclaims storage. PostgreSQL forbids this inside a transaction block.
    Vacuum,
    /// A statement PostgreSQL refuses to run inside a transaction block at all,
    /// such as `CREATE DATABASE` or `ALTER SYSTEM`.
    ///
    /// Carries the SQL spelling so a diagnostic can name what it found.
    NonTransactional(&'static str),
    /// Rebuilds an index.
    Reindex {
        /// Whether `CONCURRENTLY` was given, which PostgreSQL forbids inside a
        /// transaction block.
        concurrent: bool,
    },
    /// Anything Zapadka has no specific opinion about.
    Other,
}

impl StatementKind {
    /// Whether this statement would escape the runner's transaction boundary.
    pub fn is_transaction_control(&self) -> bool {
        matches!(self, Self::TransactionControl(_))
    }

    /// Returns the construct name when PostgreSQL refuses to run this statement
    /// inside a transaction block.
    ///
    /// Zapadka opens a transaction for `transaction = "required"` migrations, so
    /// these statements can only ever fail there. Detecting them before
    /// connecting turns a confusing mid-deploy server error into an explanation
    /// the author can act on while writing the migration.
    pub fn forbidden_in_transaction(&self) -> Option<&'static str> {
        match self {
            Self::CreateIndex {
                concurrent: true, ..
            } => Some("CREATE INDEX CONCURRENTLY"),
            Self::Drop {
                object: DropObject::Index,
                concurrent: true,
                ..
            } => Some("DROP INDEX CONCURRENTLY"),
            Self::Reindex { concurrent: true } => Some("REINDEX CONCURRENTLY"),
            Self::Vacuum => Some("VACUUM"),
            Self::NonTransactional(construct) => Some(construct),
            _ => None,
        }
    }

    /// Returns the construct name when running this statement would destroy the
    /// session state Zapadka depends on.
    ///
    /// `DISCARD ALL` is implemented partly as `pg_advisory_unlock_all()`, and
    /// Zapadka's deployment lock is session-scoped. A migration containing it
    /// would hand the lock back mid-run, letting a second deploy start while
    /// this one is still applying migrations — the exact overlap the lock
    /// exists to prevent, arrived at without anything failing.
    ///
    /// It can only be reached from the nontransactional path, since PostgreSQL
    /// refuses `DISCARD ALL` inside a transaction block on its own.
    pub fn breaks_runner_session(&self) -> Option<&'static str> {
        match self {
            Self::NonTransactional(construct @ "DISCARD") => Some(construct),
            _ => None,
        }
    }
}

/// The specific transaction-control operation a rejected statement performs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionOperation {
    /// `BEGIN` or `START TRANSACTION`.
    Begin,
    /// `COMMIT` or `END`.
    Commit,
    /// `ROLLBACK` or `ABORT`.
    Rollback,
    /// `SAVEPOINT`.
    Savepoint,
    /// `RELEASE SAVEPOINT`.
    ReleaseSavepoint,
    /// `ROLLBACK TO SAVEPOINT`.
    RollbackToSavepoint,
    /// `PREPARE TRANSACTION`, which begins two-phase commit.
    PrepareTransaction,
    /// `COMMIT PREPARED`.
    CommitPrepared,
    /// `ROLLBACK PREPARED`.
    RollbackPrepared,
    /// `SET TRANSACTION`, which changes properties of the runner's transaction.
    SetTransaction,
    /// `SET SESSION CHARACTERISTICS AS TRANSACTION`.
    SetSessionCharacteristics,
}

impl TransactionOperation {
    /// The SQL spelling to quote back to the author in a diagnostic.
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Begin => "BEGIN",
            Self::Commit => "COMMIT",
            Self::Rollback => "ROLLBACK",
            Self::Savepoint => "SAVEPOINT",
            Self::ReleaseSavepoint => "RELEASE SAVEPOINT",
            Self::RollbackToSavepoint => "ROLLBACK TO SAVEPOINT",
            Self::PrepareTransaction => "PREPARE TRANSACTION",
            Self::CommitPrepared => "COMMIT PREPARED",
            Self::RollbackPrepared => "ROLLBACK PREPARED",
            Self::SetTransaction => "SET TRANSACTION",
            Self::SetSessionCharacteristics => "SET SESSION CHARACTERISTICS",
        }
    }
}

/// A possibly schema-qualified relation name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualifiedName {
    /// The schema, when the name was qualified with one.
    pub schema: Option<String>,
    /// The object's own name.
    pub name: String,
}

impl std::fmt::Display for QualifiedName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.schema {
            Some(schema) => write!(f, "{schema}.{}", self.name),
            None => write!(f, "{}", self.name),
        }
    }
}

/// The kind of object a `DROP` statement removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropObject {
    /// A table, which holds rows.
    Table,
    /// An index, which can be rebuilt from the table.
    Index,
    /// A view, which holds no data of its own.
    View,
    /// A materialized view, which does hold data.
    MaterializedView,
    /// A sequence, whose current value cannot be recovered once dropped.
    Sequence,
    /// A schema, and by extension everything in it.
    Schema,
    /// A single column of a table.
    Column,
    /// Any other object type, named as PostgreSQL spells it.
    Other(String),
}

impl DropObject {
    /// Whether dropping this object can destroy data that no later migration
    /// can reconstruct.
    pub fn is_data_bearing(&self) -> bool {
        matches!(
            self,
            Self::Table | Self::MaterializedView | Self::Sequence | Self::Schema | Self::Column
        )
    }

    /// A human-readable object type, e.g. `table`.
    pub fn label(&self) -> String {
        match self {
            Self::Table => "table".to_owned(),
            Self::Index => "index".to_owned(),
            Self::View => "view".to_owned(),
            Self::MaterializedView => "materialized view".to_owned(),
            Self::Sequence => "sequence".to_owned(),
            Self::Schema => "schema".to_owned(),
            Self::Column => "column".to_owned(),
            Self::Other(name) => name.clone(),
        }
    }
}

/// A single action within an `ALTER TABLE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlterTableAction {
    /// Removes a column and the data in it.
    DropColumn {
        /// The column being removed.
        name: String,
        /// Whether `CASCADE` extends the drop to dependent objects.
        cascade: bool,
    },
    /// Adds a column.
    AddColumn {
        /// The column being added.
        name: String,
        /// Whether the column is declared `NOT NULL`.
        not_null: bool,
        /// Whether the column's `DEFAULT` calls a function. Zapadka cannot know
        /// whether that function is volatile without the catalog, so callers
        /// must treat this as "possibly rewrites the table", not a certainty.
        default_calls_function: bool,
    },
    /// Changes a column's type, which usually rewrites the whole table.
    AlterColumnType {
        /// The column whose type is changing.
        name: String,
    },
    /// Marks a column `NOT NULL`, which scans every existing row.
    SetNotNull {
        /// The column being constrained.
        name: String,
    },
    /// Adds a table constraint.
    AddConstraint {
        /// What kind of constraint is being added.
        kind: ConstraintKind,
        /// `NOT VALID` defers the scan of existing rows.
        not_valid: bool,
    },
    /// Validates a constraint previously added `NOT VALID`, taking a weaker
    /// lock than adding it validated would have.
    ValidateConstraint {
        /// The constraint being validated.
        name: String,
    },
    /// Any other action, named as PostgreSQL spells its parse-tree subtype.
    Other(String),
}

/// The kind of constraint an `ALTER TABLE ... ADD CONSTRAINT` adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConstraintKind {
    /// `PRIMARY KEY`.
    PrimaryKey,
    /// `UNIQUE`.
    Unique,
    /// `REFERENCES`, which also locks the referenced table.
    ForeignKey,
    /// `CHECK`.
    Check,
    /// `NOT NULL` written as a table constraint.
    NotNull,
    /// `EXCLUDE`, which is backed by an index.
    Exclusion,
    /// Any other constraint type, named as PostgreSQL spells it.
    Other(String),
}

impl ConstraintKind {
    /// A human-readable constraint type, e.g. `foreign key`.
    pub fn label(&self) -> String {
        match self {
            Self::PrimaryKey => "primary key".to_owned(),
            Self::Unique => "unique".to_owned(),
            Self::ForeignKey => "foreign key".to_owned(),
            Self::Check => "check".to_owned(),
            Self::NotNull => "not null".to_owned(),
            Self::Exclusion => "exclusion".to_owned(),
            Self::Other(name) => name.clone(),
        }
    }

    /// Whether adding this constraint scans or locks the whole table when it is
    /// added without `NOT VALID`.
    pub fn scans_existing_rows(&self) -> bool {
        matches!(
            self,
            Self::PrimaryKey | Self::Unique | Self::ForeignKey | Self::Check | Self::Exclusion
        )
    }
}

/// Converts a parse tree into [`ParsedScript`].
pub(crate) fn classify(tree: &str, script: &str) -> ParsedScript {
    // The tree came straight from `pg_query_parse`, which emits well-formed
    // JSON; a malformed tree degrades to "no statements" rather than panicking.
    let root: Value = serde_json::from_str(tree).unwrap_or(Value::Null);
    // A PG_VERSION_NUM never approaches u32::MAX; a value that did would mean
    // the tree is not a parse tree, so reporting 0 is the honest answer.
    let parser_version = root
        .get("version")
        .and_then(Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .unwrap_or_default();

    let line_index = LineIndex::new(script);
    let statements = root
        .get("stmts")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|raw| statement(raw, &line_index))
        .collect();

    ParsedScript {
        parser_version,
        statements,
    }
}

fn statement(raw: &Value, lines: &LineIndex) -> Option<Statement> {
    let node = raw.get("stmt")?.as_object()?;
    // Every statement node is a single-key object keyed by its node type.
    let (node_type, body) = node.iter().next()?;
    // Upstream omits `stmt_location` for the first statement in a script.
    // Offsets index the script that was just parsed, so they fit in a usize on
    // any target that could hold that script in memory; a value that did not is
    // not an offset, and treating it as "unknown" is better than truncating it
    // into a position that points somewhere real but wrong.
    let location = raw
        .get("stmt_location")
        .and_then(Value::as_u64)
        .and_then(|offset| usize::try_from(offset).ok())
        .unwrap_or(0);
    let length = raw
        .get("stmt_len")
        .and_then(Value::as_u64)
        .and_then(|length| usize::try_from(length).ok());

    Some(Statement {
        kind: kind(node_type, body),
        node_type: node_type.clone(),
        location,
        length,
        line: lines.line_of(location),
    })
}

fn kind(node_type: &str, body: &Value) -> StatementKind {
    match node_type {
        "TransactionStmt" => match transaction_operation(body) {
            Some(operation) => StatementKind::TransactionControl(operation),
            None => StatementKind::Other,
        },
        "VariableSetStmt" => match string(body, "name").as_deref() {
            // `SET TRANSACTION` and `SET SESSION CHARACTERISTICS AS TRANSACTION`
            // change the properties of the transaction Zapadka owns. Ordinary
            // `SET` and `SET LOCAL` of other parameters stay allowed.
            Some("TRANSACTION") => {
                StatementKind::TransactionControl(TransactionOperation::SetTransaction)
            }
            Some("SESSION CHARACTERISTICS") => {
                StatementKind::TransactionControl(TransactionOperation::SetSessionCharacteristics)
            }
            _ => StatementKind::Other,
        },
        "IndexStmt" => StatementKind::CreateIndex {
            relation: relation(body.get("relation")),
            concurrent: flag(body, "concurrent"),
            unique: flag(body, "unique"),
        },
        "CreateStmt" => StatementKind::CreateTable {
            relation: relation(body.get("relation")),
        },
        "AlterTableStmt" => StatementKind::AlterTable {
            relation: relation(body.get("relation")),
            actions: alter_table_actions(body),
        },
        "DropStmt" => StatementKind::Drop {
            object: drop_object(string(body, "removeType").as_deref()),
            cascade: cascade(body),
            concurrent: flag(body, "concurrent"),
        },
        "TruncateStmt" => StatementKind::Truncate {
            cascade: cascade(body),
        },
        "RenameStmt" => StatementKind::Rename {
            object: drop_object(string(body, "renameType").as_deref()),
            to: string(body, "newname").unwrap_or_default(),
        },
        // ANALYZE shares this node with VACUUM and is distinguished only by
        // `is_vacuumcmd`. The difference matters: ANALYZE runs happily inside a
        // transaction block, VACUUM does not.
        "VacuumStmt" if flag(body, "is_vacuumcmd") => StatementKind::Vacuum,
        // PostgreSQL refuses these inside a transaction block outright. Without
        // them here, a migration using the default transactional mode passes
        // lint, connects to the target, and only then fails -- which is exactly
        // the confusion the embedded parser exists to prevent.
        "CreatedbStmt" => StatementKind::NonTransactional("CREATE DATABASE"),
        "DropdbStmt" => StatementKind::NonTransactional("DROP DATABASE"),
        "CreateTableSpaceStmt" => StatementKind::NonTransactional("CREATE TABLESPACE"),
        "DropTableSpaceStmt" => StatementKind::NonTransactional("DROP TABLESPACE"),
        "AlterSystemStmt" => StatementKind::NonTransactional("ALTER SYSTEM"),
        "DiscardStmt" => StatementKind::NonTransactional("DISCARD"),
        "ReindexStmt" => StatementKind::Reindex {
            // `CONCURRENTLY` arrives as a named parameter rather than a flag.
            concurrent: body
                .get("params")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter_map(|param| param.get("DefElem"))
                .any(|param| string(param, "defname").as_deref() == Some("concurrently")),
        },
        _ => StatementKind::Other,
    }
}

// Several PostgreSQL keywords are synonyms. Listing them on separate arms
// documents the grammar Zapadka is matching, which is worth more than the
// shorter match clippy would prefer.
#[allow(clippy::match_same_arms)]
fn transaction_operation(body: &Value) -> Option<TransactionOperation> {
    Some(match string(body, "kind")?.as_str() {
        "TRANS_STMT_BEGIN" | "TRANS_STMT_START" => TransactionOperation::Begin,
        "TRANS_STMT_COMMIT" => TransactionOperation::Commit,
        "TRANS_STMT_ROLLBACK" => TransactionOperation::Rollback,
        "TRANS_STMT_SAVEPOINT" => TransactionOperation::Savepoint,
        "TRANS_STMT_RELEASE" => TransactionOperation::ReleaseSavepoint,
        "TRANS_STMT_ROLLBACK_TO" => TransactionOperation::RollbackToSavepoint,
        "TRANS_STMT_PREPARE" => TransactionOperation::PrepareTransaction,
        "TRANS_STMT_COMMIT_PREPARED" => TransactionOperation::CommitPrepared,
        "TRANS_STMT_ROLLBACK_PREPARED" => TransactionOperation::RollbackPrepared,
        // An unrecognized transaction statement is still transaction control.
        // Failing closed keeps a future PostgreSQL addition from slipping past
        // the guard.
        _ => TransactionOperation::Begin,
    })
}

fn alter_table_actions(body: &Value) -> Vec<AlterTableAction> {
    body.get("cmds")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|cmd| cmd.get("AlterTableCmd"))
        .map(alter_table_action)
        .collect()
}

fn alter_table_action(cmd: &Value) -> AlterTableAction {
    let subtype = string(cmd, "subtype").unwrap_or_default();
    let name = string(cmd, "name").unwrap_or_default();
    match subtype.as_str() {
        "AT_DropColumn" => AlterTableAction::DropColumn {
            name,
            cascade: cascade(cmd),
        },
        "AT_AddColumn" => {
            let column = cmd.get("def").and_then(|def| def.get("ColumnDef"));
            let constraints = column
                .and_then(|column| column.get("constraints"))
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let contypes: Vec<String> = constraints
                .iter()
                .filter_map(|c| c.get("Constraint"))
                .filter_map(|c| string(c, "contype"))
                .collect();
            let default = constraints
                .iter()
                .filter_map(|c| c.get("Constraint"))
                .find(|c| string(c, "contype").as_deref() == Some("CONSTR_DEFAULT"));
            AlterTableAction::AddColumn {
                name: column
                    .and_then(|column| string(column, "colname"))
                    .unwrap_or(name),
                not_null: contypes.iter().any(|c| c == "CONSTR_NOTNULL"),
                default_calls_function: default
                    .and_then(|d| d.get("raw_expr"))
                    .is_some_and(contains_function_call),
            }
        }
        "AT_AlterColumnType" => AlterTableAction::AlterColumnType { name },
        "AT_SetNotNull" => AlterTableAction::SetNotNull { name },
        "AT_AddConstraint" => {
            let constraint = cmd.get("def").and_then(|def| def.get("Constraint"));
            AlterTableAction::AddConstraint {
                kind: constraint_kind(constraint.and_then(|c| string(c, "contype")).as_deref()),
                // `NOT VALID` is represented as `skip_validation`.
                not_valid: constraint.is_some_and(|c| flag(c, "skip_validation")),
            }
        }
        "AT_ValidateConstraint" => AlterTableAction::ValidateConstraint { name },
        other => AlterTableAction::Other(other.to_owned()),
    }
}

fn constraint_kind(contype: Option<&str>) -> ConstraintKind {
    match contype {
        Some("CONSTR_PRIMARY") => ConstraintKind::PrimaryKey,
        Some("CONSTR_UNIQUE") => ConstraintKind::Unique,
        Some("CONSTR_FOREIGN") => ConstraintKind::ForeignKey,
        Some("CONSTR_CHECK") => ConstraintKind::Check,
        Some("CONSTR_NOTNULL") => ConstraintKind::NotNull,
        Some("CONSTR_EXCLUSION") => ConstraintKind::Exclusion,
        Some(other) => ConstraintKind::Other(other.to_owned()),
        None => ConstraintKind::Other(String::new()),
    }
}

fn drop_object(remove_type: Option<&str>) -> DropObject {
    match remove_type {
        Some("OBJECT_TABLE") => DropObject::Table,
        Some("OBJECT_INDEX") => DropObject::Index,
        Some("OBJECT_VIEW") => DropObject::View,
        Some("OBJECT_MATVIEW") => DropObject::MaterializedView,
        Some("OBJECT_SEQUENCE") => DropObject::Sequence,
        Some("OBJECT_SCHEMA") => DropObject::Schema,
        Some("OBJECT_COLUMN") => DropObject::Column,
        Some(other) => DropObject::Other(
            other
                .strip_prefix("OBJECT_")
                .unwrap_or(other)
                .to_lowercase()
                .replace('_', " "),
        ),
        None => DropObject::Other(String::new()),
    }
}

/// Whether an expression tree contains a function call anywhere.
fn contains_function_call(expr: &Value) -> bool {
    match expr {
        Value::Object(map) => map
            .iter()
            .any(|(key, value)| key == "FuncCall" || contains_function_call(value)),
        Value::Array(items) => items.iter().any(contains_function_call),
        _ => false,
    }
}

fn relation(value: Option<&Value>) -> Option<QualifiedName> {
    let value = value?;
    Some(QualifiedName {
        schema: string(value, "schemaname"),
        name: string(value, "relname")?,
    })
}

fn cascade(value: &Value) -> bool {
    string(value, "behavior").as_deref() == Some("DROP_CASCADE")
}

/// Reads a boolean field, treating an absent field as `false`.
///
/// The upstream JSON omits fields holding their zero value, so a missing
/// `concurrent` means "not concurrent".
fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

/// Precomputed newline offsets for mapping byte offsets to line numbers.
#[derive(Debug)]
struct LineIndex {
    newlines: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        Self {
            newlines: text
                .bytes()
                .enumerate()
                .filter(|(_, byte)| *byte == b'\n')
                .map(|(index, _)| index)
                .collect(),
        }
    }

    /// Returns the 1-based line containing `offset`.
    fn line_of(&self, offset: usize) -> usize {
        self.newlines.partition_point(|&newline| newline < offset) + 1
    }
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use crate::{StatementKind, TransactionOperation, parse};

    fn kinds(sql: &str) -> Vec<StatementKind> {
        parse(sql)
            .unwrap()
            .statements
            .into_iter()
            .map(|statement| statement.kind)
            .collect()
    }

    #[test]
    fn detects_every_form_of_transaction_control() {
        let cases = [
            ("BEGIN", TransactionOperation::Begin),
            ("START TRANSACTION", TransactionOperation::Begin),
            ("COMMIT", TransactionOperation::Commit),
            ("END", TransactionOperation::Commit),
            ("ROLLBACK", TransactionOperation::Rollback),
            ("ABORT", TransactionOperation::Rollback),
            ("SAVEPOINT s", TransactionOperation::Savepoint),
            (
                "RELEASE SAVEPOINT s",
                TransactionOperation::ReleaseSavepoint,
            ),
            (
                "ROLLBACK TO SAVEPOINT s",
                TransactionOperation::RollbackToSavepoint,
            ),
            (
                "PREPARE TRANSACTION 'x'",
                TransactionOperation::PrepareTransaction,
            ),
            ("COMMIT PREPARED 'x'", TransactionOperation::CommitPrepared),
            (
                "ROLLBACK PREPARED 'x'",
                TransactionOperation::RollbackPrepared,
            ),
            (
                "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
                TransactionOperation::SetTransaction,
            ),
            (
                "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY",
                TransactionOperation::SetSessionCharacteristics,
            ),
        ];
        for (sql, expected) in cases {
            assert_eq!(
                kinds(sql),
                vec![StatementKind::TransactionControl(expected)],
                "{sql}"
            );
        }
    }

    #[test]
    fn ordinary_set_is_not_transaction_control() {
        // Targets may configure `lock_timeout`; only transaction properties are
        // reserved to the runner.
        assert_eq!(
            kinds("SET LOCAL lock_timeout = '5s'"),
            vec![StatementKind::Other]
        );
        assert_eq!(kinds("SET search_path = app"), vec![StatementKind::Other]);
    }

    #[test]
    fn finds_transaction_control_after_valid_statements() {
        let script = parse("CREATE TABLE t(i int);\n\nCOMMIT;\n").unwrap();
        let offenders: Vec<_> = script.transaction_control().collect();
        assert_eq!(offenders.len(), 1);
        assert_eq!(offenders[0].line, 3);
        // Upstream statement lengths exclude the terminating semicolon.
        assert_eq!(
            offenders[0].text("CREATE TABLE t(i int);\n\nCOMMIT;\n"),
            "COMMIT"
        );
    }

    #[test]
    fn classifies_concurrent_index_creation() {
        assert_eq!(
            kinds("CREATE UNIQUE INDEX CONCURRENTLY i ON app.t (c)"),
            vec![StatementKind::CreateIndex {
                relation: Some(crate::QualifiedName {
                    schema: Some("app".to_owned()),
                    name: "t".to_owned(),
                }),
                concurrent: true,
                unique: true,
            }]
        );
    }

    #[test]
    fn classifies_alter_table_actions() {
        use crate::{AlterTableAction, ConstraintKind};
        let StatementKind::AlterTable { actions, .. } = kinds(
            "ALTER TABLE t DROP COLUMN a CASCADE, ALTER COLUMN b TYPE bigint, \
             ADD CONSTRAINT ck CHECK (b > 0) NOT VALID",
        )
        .remove(0) else {
            panic!("expected an ALTER TABLE");
        };
        assert_eq!(
            actions,
            vec![
                AlterTableAction::DropColumn {
                    name: "a".to_owned(),
                    cascade: true
                },
                AlterTableAction::AlterColumnType {
                    name: "b".to_owned()
                },
                AlterTableAction::AddConstraint {
                    kind: ConstraintKind::Check,
                    not_valid: true
                },
            ]
        );
    }

    #[test]
    fn distinguishes_constant_and_function_column_defaults() {
        use crate::AlterTableAction;
        let extract = |sql: &str| match kinds(sql).remove(0) {
            StatementKind::AlterTable { actions, .. } => actions,
            other => panic!("expected an ALTER TABLE, got {other:?}"),
        };
        assert_eq!(
            extract("ALTER TABLE t ADD COLUMN c int NOT NULL DEFAULT 0"),
            vec![AlterTableAction::AddColumn {
                name: "c".to_owned(),
                not_null: true,
                default_calls_function: false,
            }]
        );
        assert_eq!(
            extract("ALTER TABLE t ADD COLUMN c timestamptz DEFAULT now()"),
            vec![AlterTableAction::AddColumn {
                name: "c".to_owned(),
                not_null: false,
                default_calls_function: true,
            }]
        );
    }

    #[test]
    fn classifies_drops_and_truncate() {
        use crate::DropObject;
        assert_eq!(
            kinds("DROP TABLE app.t CASCADE"),
            vec![StatementKind::Drop {
                object: DropObject::Table,
                cascade: true,
                concurrent: false,
            }]
        );
        assert_eq!(
            kinds("DROP INDEX CONCURRENTLY i"),
            vec![StatementKind::Drop {
                object: DropObject::Index,
                cascade: false,
                concurrent: true,
            }]
        );
        assert_eq!(
            kinds("TRUNCATE t CASCADE"),
            vec![StatementKind::Truncate { cascade: true }]
        );
    }

    #[test]
    fn identifies_statements_postgresql_forbids_inside_a_transaction() {
        for sql in [
            "CREATE INDEX CONCURRENTLY i ON t (c)",
            "DROP INDEX CONCURRENTLY i",
            "REINDEX INDEX CONCURRENTLY i",
            "VACUUM FULL t",
            "VACUUM",
        ] {
            assert!(
                kinds(sql)[0].forbidden_in_transaction().is_some(),
                "{sql} cannot run inside a transaction block"
            );
        }
    }

    #[test]
    fn identifies_statements_that_cannot_run_in_a_transaction_at_all() {
        for (sql, construct) in [
            ("CREATE DATABASE app", "CREATE DATABASE"),
            ("DROP DATABASE app", "DROP DATABASE"),
            (
                "CREATE TABLESPACE fast LOCATION '/mnt/fast'",
                "CREATE TABLESPACE",
            ),
            ("DROP TABLESPACE fast", "DROP TABLESPACE"),
            ("ALTER SYSTEM SET work_mem = '4MB'", "ALTER SYSTEM"),
            ("DISCARD ALL", "DISCARD"),
        ] {
            assert_eq!(
                kinds(sql)[0].forbidden_in_transaction(),
                Some(construct),
                "{sql}"
            );
        }
    }

    #[test]
    fn analyze_is_not_mistaken_for_vacuum() {
        // PostgreSQL parses both into VacuumStmt, but ANALYZE is allowed inside
        // a transaction block. Treating it as VACUUM would reject valid
        // migrations that refresh statistics after a bulk change.
        assert_eq!(kinds("ANALYZE t"), vec![StatementKind::Other]);
        assert_eq!(kinds("ANALYZE"), vec![StatementKind::Other]);
        assert_eq!(kinds("VACUUM ANALYZE t"), vec![StatementKind::Vacuum]);
    }

    #[test]
    fn ordinary_forms_of_those_statements_are_transactional() {
        // Only the concurrent variants are restricted; the plain ones are fine
        // inside the runner's transaction.
        for sql in [
            "CREATE INDEX i ON t (c)",
            "DROP INDEX i",
            "REINDEX INDEX i",
            "DROP TABLE t",
        ] {
            assert_eq!(kinds(sql)[0].forbidden_in_transaction(), None, "{sql}");
        }
    }

    #[test]
    fn classifies_renames_that_break_running_application_code() {
        use crate::DropObject;
        assert_eq!(
            kinds("ALTER TABLE t RENAME COLUMN a TO b"),
            vec![StatementKind::Rename {
                object: DropObject::Column,
                to: "b".to_owned()
            }]
        );
        assert_eq!(
            kinds("ALTER TABLE t RENAME TO t2"),
            vec![StatementKind::Rename {
                object: DropObject::Table,
                to: "t2".to_owned()
            }]
        );
    }

    #[test]
    fn unknown_statements_do_not_become_transaction_control() {
        // Zapadka must not reject SQL simply because this module has no opinion.
        assert_eq!(
            kinds("CREATE EXTENSION IF NOT EXISTS pgcrypto"),
            vec![StatementKind::Other]
        );
        assert_eq!(kinds("ANALYZE t"), vec![StatementKind::Other]);
    }
}
