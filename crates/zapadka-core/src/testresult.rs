//! What a test file did, as facts rather than as text.
//!
//! This replaces the TAP document. The difference is not cosmetic: a TAP
//! document was *reconstructed* from prose the server printed, so every field
//! was a guess that a strict parser had to defend. These values are read from
//! typed columns, so the only way to get them wrong is to write the wrong
//! query.
//!
//! The runner still validates what it reads. Not because the columns might lie,
//! but because a gap between what the assertion library recorded and what the
//! runner sees would mean something is broken — and silently reporting fewer
//! assertions than actually ran is the one failure mode worth engineering
//! against.

use serde_json::Value;

/// Everything one test file produced.
#[derive(Debug, Clone, Default)]
pub struct TestDocument {
    /// The plan the file declared, if it declared one.
    pub plan: Option<PlanDeclaration>,
    /// Whether the file called `finish()`.
    pub finished: bool,
    /// Assertions in the order they ran.
    pub assertions: Vec<Assertion>,
    /// Free-standing notes, in the order they were written.
    pub notes: Vec<Note>,
}

/// A declared plan.
///
/// Absent is normal: declaring a count is optional, because a runner reading a
/// table already knows whether the file finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanDeclaration {
    /// `plan(n)` — the file expects exactly this many assertions.
    Count(u32),
    /// `no_plan()` — the file declines to say.
    NoPlan,
}

/// One recorded assertion.
#[derive(Debug, Clone)]
pub struct Assertion {
    /// 1-based position within the file.
    pub number: u32,
    /// Which assertion function recorded it, e.g. `is` or `has_table`.
    pub kind: String,
    /// Whether the comparison actually held. A TODO failure is `false` here;
    /// whether it fails the *run* is decided by the directive.
    pub passed: bool,
    /// The description the author gave, when they gave one.
    pub description: Option<String>,
    /// A TODO or SKIP directive.
    pub directive: Option<Directive>,
    /// Family-specific failure detail: operands and their types, differing
    /// rows, missing privileges. Absent when the assertion passed.
    pub detail: Option<Value>,
}

/// Why an assertion's outcome should not be taken at face value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Directive {
    /// Expected to fail; a failure does not fail the run.
    Todo(String),
    /// Not run at all.
    Skip(String),
}

/// A note written with `diag()`.
#[derive(Debug, Clone)]
pub struct Note {
    /// The assertion this note followed, if any. `None` means it was written
    /// before the first assertion and belongs to no assertion — pretending
    /// otherwise would attach it to something arbitrary.
    pub after_assertion: Option<u32>,
    /// The text.
    pub message: String,
}

impl Assertion {
    /// Whether this assertion should fail the file.
    ///
    /// A TODO failure is expected, and a skip did not run. Only an undirected
    /// failure counts.
    pub fn fails_the_run(&self) -> bool {
        !self.passed && self.directive.is_none()
    }
}

impl TestDocument {
    /// Whether the file passed.
    pub fn passed(&self) -> bool {
        !self.assertions.iter().any(Assertion::fails_the_run)
    }

    /// The count a report should show, when the file declared one.
    pub fn planned(&self) -> Option<u32> {
        match self.plan {
            Some(PlanDeclaration::Count(count)) => Some(count),
            _ => None,
        }
    }

    /// Checks the document is internally consistent.
    ///
    /// The assertion library numbers rows consecutively from one. A gap means
    /// rows were lost between the library recording them and the runner reading
    /// them, and a missing failure is exactly the kind of loss that looks like
    /// success. A declared plan that disagrees with the count means the file
    /// did not run the assertions its author expected — pgTAP could only report
    /// that as a diagnostic, so it never actually failed a run.
    pub fn validate(&self) -> Result<(), String> {
        for (index, assertion) in self.assertions.iter().enumerate() {
            let expected = u32::try_from(index + 1).unwrap_or(u32::MAX);
            if assertion.number != expected {
                return Err(format!(
                    "assertion numbering jumps from {expected} to {}; results were lost between \
                     the database and this report",
                    assertion.number
                ));
            }
        }

        if let Some(PlanDeclaration::Count(planned)) = self.plan {
            let ran = u32::try_from(self.assertions.len()).unwrap_or(u32::MAX);
            if planned != ran {
                return Err(format!(
                    "the file planned {planned} assertion(s) but ran {ran}"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // Assertions and unreachable branches in tests panic by design.
    #![allow(clippy::panic)]

    use super::*;

    fn assertion(number: u32, passed: bool, directive: Option<Directive>) -> Assertion {
        Assertion {
            number,
            kind: "is".to_owned(),
            passed,
            description: None,
            directive,
            detail: None,
        }
    }

    #[test]
    fn an_undirected_failure_fails_the_run() {
        let document = TestDocument {
            assertions: vec![assertion(1, true, None), assertion(2, false, None)],
            ..TestDocument::default()
        };
        assert!(!document.passed());
    }

    #[test]
    fn a_todo_failure_does_not_fail_the_run() {
        let document = TestDocument {
            assertions: vec![assertion(1, false, Some(Directive::Todo("later".into())))],
            ..TestDocument::default()
        };
        assert!(document.passed(), "a TODO is expected to fail");
    }

    #[test]
    fn a_skip_does_not_fail_the_run() {
        let document = TestDocument {
            assertions: vec![assertion(1, true, Some(Directive::Skip("no data".into())))],
            ..TestDocument::default()
        };
        assert!(document.passed());
    }

    #[test]
    fn a_gap_in_numbering_is_rejected() {
        // The library numbers from one without gaps, so a gap means rows went
        // missing on the way here -- and a missing failure reads as success.
        let document = TestDocument {
            assertions: vec![assertion(1, true, None), assertion(3, true, None)],
            ..TestDocument::default()
        };
        let error = document.validate().unwrap_err();
        assert!(error.contains("numbering jumps"), "{error}");
    }

    #[test]
    fn a_plan_that_disagrees_with_the_count_is_rejected() {
        let document = TestDocument {
            plan: Some(PlanDeclaration::Count(3)),
            assertions: vec![assertion(1, true, None)],
            ..TestDocument::default()
        };
        let error = document.validate().unwrap_err();
        assert!(error.contains("planned 3"), "{error}");
    }

    #[test]
    fn no_plan_never_disagrees() {
        let document = TestDocument {
            plan: Some(PlanDeclaration::NoPlan),
            assertions: vec![assertion(1, true, None)],
            ..TestDocument::default()
        };
        assert!(document.validate().is_ok());
    }
}
