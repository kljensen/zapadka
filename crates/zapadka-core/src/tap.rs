//! A strict TAP 13 subset, as pgTAP emits it.
//!
//! TAP is Zapadka's internal transport, not its output. `zapadka test` reads it
//! from the database and turns it into the same [`crate::report::ReportV1`]
//! every other command produces; nobody consuming Zapadka ever sees TAP.
//!
//! # Why strict
//!
//! A test framework's whole value is that a pass means something. Every
//! ambiguity this parser tolerated would be a way for a broken test file to
//! report success: a missing plan, a count that does not match, a duplicated
//! assertion number, a stray line that might be output or might be a swallowed
//! failure.
//!
//! So this parser accepts exactly what pgTAP produces and rejects everything
//! else, including parts of TAP 13 that pgTAP does not emit. Rejecting a valid
//! TAP document Zapadka has never seen is a much better failure than accepting
//! a malformed one and calling it green.
//!
//! Specifically rejected: a missing or duplicated plan, a plan in the middle, a
//! result count that disagrees with the plan, numbers that are out of order or
//! repeated, `Bail out!`, subtests, pragmas, and any line that is not a plan, a
//! result, a comment, or a YAML diagnostic block.

use std::collections::BTreeMap;
use std::fmt;

/// A parsed TAP stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapDocument {
    /// The plan the file declared.
    pub plan: Plan,
    /// Assertions in the order they were emitted.
    pub assertions: Vec<TapAssertion>,
}

impl TapDocument {
    /// Whether the file passed as a whole.
    ///
    /// A `TODO` failure does not fail the file — that is what `TODO` means. A
    /// `TODO` that unexpectedly passed does not fail it either, but it is worth
    /// reporting, because a stale `TODO` hides a test nobody is reading.
    pub fn passed(&self) -> bool {
        !self
            .assertions
            .iter()
            .any(|assertion| assertion.outcome == Outcome::Failed)
    }

    /// Assertions that failed and were not marked `TODO`.
    pub fn failures(&self) -> impl Iterator<Item = &TapAssertion> {
        self.assertions
            .iter()
            .filter(|assertion| assertion.outcome == Outcome::Failed)
    }
}

/// The plan a file declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// `1..N` — the file promised exactly this many assertions.
    Count(u64),
    /// `1..0 # SKIP <reason>` — the file declined to run.
    SkipAll(String),
}

/// One assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapAssertion {
    /// 1-based position, as the file numbered it.
    pub number: u64,
    /// The description, when one was given.
    pub description: Option<String>,
    /// What the assertion did.
    pub outcome: Outcome,
    /// The reason attached to a `TODO` or `SKIP` directive.
    pub directive_reason: Option<String>,
    /// Fields from an attached YAML diagnostic block, such as `have` and
    /// `want`.
    pub diagnostics: BTreeMap<String, String>,
}

/// The result of one assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The assertion held.
    Passed,
    /// Failed, and not excused by a directive. Fails the file.
    Failed,
    /// Failed while marked `TODO`. Expected, so it does not fail the file.
    TodoFailed,
    /// Passed while marked `TODO`, which usually means the `TODO` is stale.
    TodoPassed,
    /// Not run, because it was marked `SKIP`.
    Skipped,
}

/// A TAP stream Zapadka refused to interpret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct TapError {
    /// What was wrong.
    pub kind: TapErrorKind,
    /// 1-based line in the TAP stream, when the problem has one.
    pub line: Option<usize>,
}

impl fmt::Display for TapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "{} (TAP line {line})", self.kind),
            None => write!(f, "{}", self.kind),
        }
    }
}

/// Why a TAP stream was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TapErrorKind {
    /// No `1..N` line at all.
    #[error("the test emitted no plan; a file must call plan(...) or no_plan() and finish()")]
    MissingPlan,
    /// More than one `1..N` line.
    #[error("the test emitted more than one plan")]
    DuplicatePlan,
    /// A plan appeared between results rather than before or after them.
    #[error("the plan appeared in the middle of the results")]
    PlanNotAtEdge,
    /// The plan and the number of results disagree.
    #[error("the plan promised {planned} assertion(s) but the test emitted {emitted}")]
    CountMismatch {
        /// What the `1..N` line promised.
        planned: u64,
        /// How many results actually arrived.
        emitted: u64,
    },
    /// A result was numbered out of sequence.
    #[error("assertion numbered {found} where {expected} was expected")]
    NumberOutOfSequence {
        /// The next number in sequence.
        expected: u64,
        /// The number the file used.
        found: u64,
    },
    /// `1..0 # SKIP` followed by results.
    #[error("the test declared it was skipping everything but emitted {emitted} assertion(s)")]
    SkippedButRan {
        /// How many results arrived despite the skip.
        emitted: u64,
    },
    /// `Bail out!` — the file abandoned the run.
    #[error("the test bailed out: {0}")]
    BailedOut(String),
    /// A construct this parser deliberately does not accept.
    #[error("{0} is not supported; Zapadka accepts the TAP subset pgTAP emits")]
    Unsupported(String),
    /// A line that is not a plan, a result, a comment, or a diagnostic.
    #[error("unrecognized TAP output: {0:?}")]
    Unrecognized(String),
}

impl TapError {
    fn at(kind: TapErrorKind, line: usize) -> Self {
        Self {
            kind,
            line: Some(line),
        }
    }

    fn whole(kind: TapErrorKind) -> Self {
        Self { kind, line: None }
    }
}

/// Parses a TAP stream.
pub fn parse(text: &str) -> Result<TapDocument, TapError> {
    let mut parser = Parser::default();
    for (index, raw) in text.lines().enumerate() {
        parser.line(raw, index + 1)?;
    }
    parser.finish()
}

#[derive(Default)]
struct Parser {
    plan: Option<Plan>,
    /// Whether the plan came before any result. A trailing plan is equally
    /// valid; a plan in between is not.
    plan_before_results: bool,
    plan_line: Option<usize>,
    assertions: Vec<TapAssertion>,
    /// Indentation of the YAML block currently being read, if any.
    yaml: Option<YamlBlock>,
}

struct YamlBlock {
    indent: usize,
    fields: BTreeMap<String, String>,
}

impl Parser {
    fn line(&mut self, raw: &str, line: usize) -> Result<(), TapError> {
        if self.yaml.is_some() {
            return self.yaml_line(raw, line);
        }

        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(());
        }

        if let Some(comment) = trimmed.strip_prefix('#') {
            self.comment(comment);
            return Ok(());
        }

        if let Some(rest) = trimmed.strip_prefix("Bail out!") {
            return Err(TapError::at(
                TapErrorKind::BailedOut(rest.trim().to_owned()),
                line,
            ));
        }

        for unsupported in ["pragma ", "TAP version", "1..0 # skip"] {
            if trimmed.starts_with(unsupported) && !trimmed.starts_with("1..0 # SKIP") {
                return Err(TapError::at(
                    TapErrorKind::Unsupported(unsupported.trim().to_owned()),
                    line,
                ));
            }
        }

        if trimmed.starts_with("1..") {
            return self.plan_line(trimmed, line);
        }

        // A leading `    ` marks a subtest in TAP 13. pgTAP does not emit them,
        // and silently flattening one would merge a nested file's results into
        // this one's count.
        if raw.starts_with("    ") && (trimmed.starts_with("ok") || trimmed.starts_with("not ok")) {
            return Err(TapError::at(
                TapErrorKind::Unsupported("a subtest".to_owned()),
                line,
            ));
        }

        if trimmed == "---" {
            // A YAML block belongs to the assertion before it.
            if self.assertions.is_empty() {
                return Err(TapError::at(
                    TapErrorKind::Unrecognized(trimmed.to_owned()),
                    line,
                ));
            }
            self.yaml = Some(YamlBlock {
                indent: raw.len() - raw.trim_start().len(),
                fields: BTreeMap::new(),
            });
            return Ok(());
        }

        self.result_line(trimmed, line)
    }

    /// Reads a comment line.
    ///
    /// pgTAP reports the detail of a failure in comments rather than in the
    /// YAML block TAP 13 defines:
    ///
    /// ```text
    /// not ok 1 - the totals match
    /// # Failed test 1: "the totals match"
    /// #         have: 41
    /// #         want: 42
    /// ```
    ///
    /// Those two lines are the most useful thing in a failing run, so they are
    /// attached to the assertion they describe rather than discarded. The
    /// surrounding prose — `Failed test 1:` and the closing `Looks like you
    /// failed ...` summary — carries nothing the report does not already have.
    fn comment(&mut self, comment: &str) {
        // pgTAP indents its diagnostic fields well past the prose around them:
        // `#         have: 1` against `# Failed test 1: "..."`. Requiring the
        // indentation is what keeps an ordinary `# note: ...` written by a test
        // author out of the report.
        let indent = comment.len() - comment.trim_start().len();
        if indent < 2 {
            return;
        }

        let Some((key, value)) = comment.trim().split_once(':') else {
            return;
        };
        // A single word before the colon, so a sentence that happens to contain
        // a colon is not read as a field.
        let key = key.trim();
        if key.is_empty() || key.contains(char::is_whitespace) {
            return;
        }
        // Comments before any result describe the file, not an assertion.
        if let Some(assertion) = self.assertions.last_mut() {
            assertion
                .diagnostics
                .insert(key.to_owned(), value.trim().to_owned());
        }
    }

    /// Reads `1..N` or `1..0 # SKIP <reason>`.
    fn plan_line(&mut self, trimmed: &str, line: usize) -> Result<(), TapError> {
        if self.plan.is_some() {
            return Err(TapError::at(TapErrorKind::DuplicatePlan, line));
        }

        let rest = &trimmed[3..];
        let (count, directive) = match rest.split_once('#') {
            Some((count, directive)) => (count.trim(), Some(directive.trim())),
            None => (rest.trim(), None),
        };
        let count: u64 = count
            .parse()
            .map_err(|_| TapError::at(TapErrorKind::Unrecognized(trimmed.to_owned()), line))?;

        self.plan = Some(match (count, directive) {
            (0, Some(directive)) => {
                let reason = directive
                    .strip_prefix("SKIP")
                    .or_else(|| directive.strip_prefix("skip"))
                    .ok_or_else(|| {
                        TapError::at(TapErrorKind::Unrecognized(trimmed.to_owned()), line)
                    })?;
                Plan::SkipAll(reason.trim().to_owned())
            }
            (0, None) => Plan::SkipAll(String::new()),
            (count, _) => Plan::Count(count),
        });
        self.plan_before_results = self.assertions.is_empty();
        self.plan_line = Some(line);
        Ok(())
    }

    /// Reads `ok`/`not ok`, with an optional number, description, and directive.
    fn result_line(&mut self, trimmed: &str, line: usize) -> Result<(), TapError> {
        // The keyword has to end at a token boundary. Matching a bare prefix
        // would read `okay` as a passing assertion followed by `ay`, so a
        // stray single-column result could turn a broken file green.
        let Some((passed, rest)) = result_keyword(trimmed) else {
            return Err(TapError::at(
                TapErrorKind::Unrecognized(trimmed.to_owned()),
                line,
            ));
        };

        // A plan already seen and results now arriving means the plan was not
        // at an edge, unless it came first.
        if self.plan.is_some() && !self.plan_before_results {
            return Err(TapError::at(TapErrorKind::PlanNotAtEdge, line));
        }

        let rest = rest.trim_start();
        // The number is optional in TAP; pgTAP always emits it. When it is
        // present it must be the next one in sequence.
        let (number, rest) = split_number(rest);
        let expected = self.assertions.len() as u64 + 1;
        let number = match number {
            Some(number) if number != expected => {
                return Err(TapError::at(
                    TapErrorKind::NumberOutOfSequence {
                        expected,
                        found: number,
                    },
                    line,
                ));
            }
            Some(number) => number,
            None => expected,
        };

        let (description, directive) = split_directive(rest.trim());
        let outcome = match (passed, directive.as_ref()) {
            (true, Some(Directive::Todo(_))) => Outcome::TodoPassed,
            (false, Some(Directive::Todo(_))) => Outcome::TodoFailed,
            (_, Some(Directive::Skip(_))) => Outcome::Skipped,
            (true, None) => Outcome::Passed,
            (false, None) => Outcome::Failed,
        };

        self.assertions.push(TapAssertion {
            number,
            description: (!description.is_empty()).then(|| description.to_owned()),
            outcome,
            directive_reason: directive.map(Directive::into_reason),
            diagnostics: BTreeMap::new(),
        });
        Ok(())
    }

    /// Reads one line of an open YAML diagnostic block.
    fn yaml_line(&mut self, raw: &str, line: usize) -> Result<(), TapError> {
        let Some(block) = &mut self.yaml else {
            return Ok(());
        };
        let trimmed = raw.trim();

        if trimmed == "..." {
            let fields = std::mem::take(&mut block.fields);
            self.yaml = None;
            if let Some(assertion) = self.assertions.last_mut() {
                assertion.diagnostics = fields;
            }
            return Ok(());
        }

        // Deliberately not a YAML parser. pgTAP emits a flat map of scalars,
        // and accepting arbitrary YAML would mean taking on a parser far larger
        // and riskier than the thing it is describing.
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim();
            if !key.is_empty() {
                block
                    .fields
                    .insert(key.to_owned(), value.trim().trim_matches('\'').to_owned());
            }
            return Ok(());
        }

        // A continuation or an empty line inside the block is ignored; anything
        // else is a shape this parser does not claim to understand.
        if trimmed.is_empty() || raw.len() > block.indent {
            return Ok(());
        }
        Err(TapError::at(
            TapErrorKind::Unrecognized(trimmed.to_owned()),
            line,
        ))
    }

    /// Validates the stream as a whole.
    fn finish(self) -> Result<TapDocument, TapError> {
        if self.yaml.is_some() {
            return Err(TapError::whole(TapErrorKind::Unrecognized(
                "an unterminated YAML diagnostic block".to_owned(),
            )));
        }

        let plan = self.plan.ok_or_else(|| {
            // The single most important check here. A file whose SQL errored
            // partway through emits some passing results and no plan; without
            // this it would look like a pass.
            TapError::whole(TapErrorKind::MissingPlan)
        })?;

        let emitted = self.assertions.len() as u64;
        match &plan {
            Plan::SkipAll(_) if emitted > 0 => {
                return Err(TapError::whole(TapErrorKind::SkippedButRan { emitted }));
            }
            Plan::Count(planned) if *planned != emitted => {
                return Err(TapError::whole(TapErrorKind::CountMismatch {
                    planned: *planned,
                    emitted,
                }));
            }
            _ => {}
        }

        Ok(TapDocument {
            plan,
            assertions: self.assertions,
        })
    }
}

/// A `# TODO` or `# SKIP` directive.
enum Directive {
    Todo(String),
    Skip(String),
}

impl Directive {
    fn into_reason(self) -> String {
        match self {
            Self::Todo(reason) | Self::Skip(reason) => reason,
        }
    }
}

/// Splits `ok` or `not ok` from the rest of the line.
///
/// The keyword must be the whole token: followed by whitespace, a `#`, or the
/// end of the line. TAP has no other terminator, and accepting a bare prefix is
/// how `okay` becomes a passing assertion.
fn result_keyword(trimmed: &str) -> Option<(bool, &str)> {
    for (keyword, passed) in [("not ok", false), ("ok", true)] {
        let Some(rest) = trimmed.strip_prefix(keyword) else {
            continue;
        };
        let ends_token = rest
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == '#');
        if ends_token {
            return Some((passed, rest));
        }
    }
    None
}

/// Splits a leading assertion number from the rest of the line.
fn split_number(rest: &str) -> (Option<u64>, &str) {
    let digits = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if digits == 0 {
        return (None, rest);
    }
    match rest[..digits].parse() {
        Ok(number) => (Some(number), &rest[digits..]),
        Err(_) => (None, rest),
    }
}

/// Splits a description from a trailing `# TODO`/`# SKIP` directive.
fn split_directive(rest: &str) -> (&str, Option<Directive>) {
    let rest = rest.strip_prefix('-').unwrap_or(rest).trim();
    let Some(hash) = rest.find('#') else {
        return (rest, None);
    };

    let (description, directive) = rest.split_at(hash);
    let directive = directive[1..].trim();
    let upper = directive.to_uppercase();

    let parsed = if let Some(reason) = upper.strip_prefix("TODO") {
        Some(Directive::Todo(
            directive[directive.len() - reason.len()..]
                .trim()
                .to_owned(),
        ))
    } else if let Some(reason) = upper.strip_prefix("SKIP") {
        Some(Directive::Skip(
            directive[directive.len() - reason.len()..]
                .trim()
                .to_owned(),
        ))
    } else {
        // A `#` that is not a directive is part of the description, which is
        // how a test describing `# of rows` keeps its description intact.
        return (rest, None);
    };

    (description.trim(), parsed)
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;

    fn ok(text: &str) -> TapDocument {
        parse(text).unwrap_or_else(|error| panic!("expected valid TAP, got {error}\n{text}"))
    }

    fn err(text: &str) -> TapErrorKind {
        parse(text)
            .map(|document| panic!("expected an error, parsed {document:?}"))
            .unwrap_err()
            .kind
    }

    #[test]
    fn parses_a_passing_file() {
        let document = ok("1..2\nok 1 - the table exists\nok 2 - it has three columns\n");
        assert_eq!(document.plan, Plan::Count(2));
        assert_eq!(document.assertions.len(), 2);
        assert_eq!(
            document.assertions[0].description.as_deref(),
            Some("the table exists")
        );
        assert!(document.passed());
    }

    #[test]
    fn a_trailing_plan_is_as_valid_as_a_leading_one() {
        // `no_plan()` plus `finish()` emits the plan last.
        let document = ok("ok 1 - first\nok 2 - second\n1..2\n");
        assert_eq!(document.plan, Plan::Count(2));
        assert!(document.passed());
    }

    #[test]
    fn a_failure_fails_the_file() {
        let document = ok("1..2\nok 1 - fine\nnot ok 2 - broken\n");
        assert!(!document.passed());
        let failures: Vec<_> = document.failures().collect();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].number, 2);
    }

    #[test]
    fn a_missing_plan_is_rejected() {
        // This is the case that matters most: a file whose SQL errored partway
        // through emits passing results and no plan, and would otherwise look
        // like a pass.
        assert_eq!(err("ok 1 - one\nok 2 - two\n"), TapErrorKind::MissingPlan);
        assert_eq!(err(""), TapErrorKind::MissingPlan);
    }

    #[test]
    fn a_count_that_disagrees_with_the_plan_is_rejected() {
        // A file that promised five assertions and emitted two stopped early.
        assert_eq!(
            err("1..5\nok 1\nok 2\n"),
            TapErrorKind::CountMismatch {
                planned: 5,
                emitted: 2
            }
        );
        assert_eq!(
            err("1..1\nok 1\nok 2\n"),
            TapErrorKind::CountMismatch {
                planned: 1,
                emitted: 2
            }
        );
    }

    #[test]
    fn numbering_must_be_sequential() {
        assert_eq!(
            err("1..2\nok 1\nok 3\n"),
            TapErrorKind::NumberOutOfSequence {
                expected: 2,
                found: 3
            }
        );
        // A duplicate is the same failure seen from the other side.
        assert_eq!(
            err("1..2\nok 1\nok 1\n"),
            TapErrorKind::NumberOutOfSequence {
                expected: 2,
                found: 1
            }
        );
    }

    #[test]
    fn unnumbered_results_are_accepted_and_numbered_in_order() {
        let document = ok("1..2\nok - first\nok - second\n");
        assert_eq!(document.assertions[0].number, 1);
        assert_eq!(document.assertions[1].number, 2);
    }

    #[test]
    fn duplicate_plans_are_rejected() {
        assert_eq!(err("1..1\nok 1\n1..1\n"), TapErrorKind::DuplicatePlan);
    }

    #[test]
    fn a_plan_in_the_middle_is_rejected() {
        assert_eq!(err("ok 1\n1..2\nok 2\n"), TapErrorKind::PlanNotAtEdge);
    }

    #[test]
    fn todo_failures_do_not_fail_the_file() {
        let document = ok("1..2\nok 1 - works\nnot ok 2 - broken # TODO not implemented yet\n");
        assert!(
            document.passed(),
            "a TODO failure is expected, not a failure"
        );
        assert_eq!(document.assertions[1].outcome, Outcome::TodoFailed);
        assert_eq!(
            document.assertions[1].directive_reason.as_deref(),
            Some("not implemented yet")
        );
    }

    #[test]
    fn a_todo_that_passes_is_distinguished_from_an_ordinary_pass() {
        // Usually means the TODO is stale, which is worth surfacing.
        let document = ok("1..1\nok 1 - works now # TODO should still be broken\n");
        assert_eq!(document.assertions[0].outcome, Outcome::TodoPassed);
        assert!(document.passed());
    }

    #[test]
    fn skipped_assertions_are_neither_passes_nor_failures() {
        let document = ok("1..2\nok 1 - ran\nok 2 - not run # SKIP needs the extension\n");
        assert_eq!(document.assertions[1].outcome, Outcome::Skipped);
        assert_eq!(
            document.assertions[1].directive_reason.as_deref(),
            Some("needs the extension")
        );
        assert!(document.passed());
    }

    #[test]
    fn a_file_can_skip_everything() {
        let document = ok("1..0 # SKIP the extension is not installed\n");
        assert_eq!(
            document.plan,
            Plan::SkipAll("the extension is not installed".to_owned())
        );
        assert!(document.assertions.is_empty());
        assert!(document.passed());
    }

    #[test]
    fn a_file_that_skipped_everything_may_not_then_run_something() {
        assert_eq!(
            err("1..0 # SKIP nothing to do\nok 1 - but here we are\n"),
            TapErrorKind::SkippedButRan { emitted: 1 }
        );
    }

    #[test]
    fn yaml_diagnostics_are_attached_to_their_assertion() {
        let document = ok("1..1\n\
             not ok 1 - the totals match\n\
             # Failed test 1\n\
             ---\n\
             have: 41\n\
             want: 42\n\
             ...\n");
        let assertion = &document.assertions[0];
        assert_eq!(assertion.diagnostics["have"], "41");
        assert_eq!(assertion.diagnostics["want"], "42");
    }

    #[test]
    fn an_unterminated_yaml_block_is_rejected() {
        let kind = err("1..1\nnot ok 1 - broken\n---\nhave: 41\n");
        assert!(matches!(kind, TapErrorKind::Unrecognized(_)), "{kind:?}");
    }

    #[test]
    fn bailing_out_is_reported_rather_than_treated_as_the_end() {
        assert_eq!(
            err("1..5\nok 1\nBail out! the database went away\n"),
            TapErrorKind::BailedOut("the database went away".to_owned())
        );
    }

    #[test]
    fn subtests_are_rejected_rather_than_flattened() {
        // Flattening would merge a nested file's results into this one's count.
        let kind = err("1..1\n    ok 1 - inner\n    1..1\nok 1 - outer\n");
        assert!(matches!(kind, TapErrorKind::Unsupported(_)), "{kind:?}");
    }

    #[test]
    fn a_word_merely_starting_with_ok_is_not_an_assertion() {
        // `okay` read as a passing assertion is how a stray single-column
        // result turns a broken file green.
        for text in ["1..1\nokay\n", "1..1\nokey dokey\n", "1..1\nnot okay\n"] {
            let kind = err(text);
            assert!(
                matches!(kind, TapErrorKind::Unrecognized(_)),
                "{text:?}: {kind:?}"
            );
        }
    }

    #[test]
    fn the_keyword_may_end_the_line_or_be_followed_by_a_directive() {
        assert_eq!(ok("1..1\nok\n").assertions[0].outcome, Outcome::Passed);
        assert_eq!(
            ok("1..1\nok # SKIP nothing to do\n").assertions[0].outcome,
            Outcome::Skipped
        );
    }

    #[test]
    fn stray_output_is_rejected_rather_than_ignored() {
        // A stray line might be harmless, or it might be a swallowed failure.
        // Zapadka cannot tell, so it declines to guess.
        let kind = err("1..1\nNOTICE:  something happened\nok 1 - fine\n");
        assert!(matches!(kind, TapErrorKind::Unrecognized(_)), "{kind:?}");
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let document = ok("# starting\n\n1..1\n\n# about to assert\nok 1 - fine\n# done\n");
        assert_eq!(document.assertions.len(), 1);
        assert!(document.assertions[0].diagnostics.is_empty());
    }

    #[test]
    fn pgtap_comment_diagnostics_are_attached_to_their_assertion() {
        // This is the shape pgTAP actually emits, and `have`/`want` is the most
        // useful thing in a failing run.
        let document = ok("1..1\n\
             not ok 1 - one equals two\n\
             # Failed test 1: \"one equals two\"\n\
             #         have: 1\n\
             #         want: 2\n\
             # Looks like you failed 1 test of 1\n");
        let assertion = &document.assertions[0];
        assert_eq!(assertion.diagnostics["have"], "1");
        assert_eq!(assertion.diagnostics["want"], "2");
        // The surrounding prose is not mistaken for a field.
        assert_eq!(
            assertion.diagnostics.len(),
            2,
            "{:?}",
            assertion.diagnostics
        );
    }

    #[test]
    fn prose_containing_a_colon_is_not_mistaken_for_a_diagnostic() {
        let document = ok("1..1\nok 1 - fine\n# note: this is prose, not a field\n");
        assert!(
            document.assertions[0].diagnostics.is_empty(),
            "{:?}",
            document.assertions[0].diagnostics
        );
    }

    #[test]
    fn a_hash_in_a_description_is_not_mistaken_for_a_directive() {
        let document = ok("1..1\nok 1 - counts the # of rows\n");
        assert_eq!(
            document.assertions[0].description.as_deref(),
            Some("counts the # of rows")
        );
        assert_eq!(document.assertions[0].outcome, Outcome::Passed);
    }

    #[test]
    fn directives_are_case_insensitive() {
        for directive in ["# TODO later", "# todo later", "# ToDo later"] {
            let document = ok(&format!("1..1\nnot ok 1 - x {directive}\n"));
            assert_eq!(document.assertions[0].outcome, Outcome::TodoFailed);
        }
    }

    #[test]
    fn a_description_may_be_absent() {
        let document = ok("1..2\nok 1\nnot ok 2\n");
        assert_eq!(document.assertions[0].description, None);
        assert!(!document.passed());
    }

    #[test]
    fn parsing_never_panics_on_arbitrary_input() {
        // The TAP stream comes from a database, and a malformed one must
        // produce a classified error rather than take the process down.
        for text in [
            "1..",
            "1..abc",
            "ok",
            "not ok",
            "1..0 #",
            "1..0 # SKIP",
            "---",
            "...",
            "ok 99999999999999999999999",
            "1..99999999999999999999999",
            "\u{0}\u{1}\u{2}",
            "ok 1 - \u{1f600}",
            "not ok 1 - x # TODO",
        ] {
            let _ = parse(text);
        }
    }
}
