//! Sqllogictest runner.

use crate::parser::*;
use async_trait::async_trait;
use itertools::Itertools;
use std::{path::Path, rc::Rc};
use tempfile::{tempdir, TempDir};

/// The async database to be tested.
#[async_trait]
pub trait AsyncDB {
    /// The error type of SQL execution.
    type Error: std::error::Error + 'static;

    /// Async run a SQL query and return the output.
    async fn run(&self, sql: &str) -> Result<String, Self::Error>;
}

/// The database to be tested.
pub trait DB {
    /// The error type of SQL execution.
    type Error: std::error::Error + 'static;

    /// Run a SQL query and return the output.
    fn run(&self, sql: &str) -> Result<String, Self::Error>;
}

/// The error type for running sqllogictest.
#[derive(thiserror::Error, Debug, Clone)]
#[error("test error at {loc}: {kind}")]
pub struct TestError {
    kind: TestErrorKind,
    loc: Location,
}

impl TestError {
    /// Returns the corresponding [`TestErrorKind`] for this error.
    pub fn kind(&self) -> TestErrorKind {
        self.kind.clone()
    }

    /// Returns the location from which the error originated.
    pub fn location(&self) -> Location {
        self.loc.clone()
    }
}

/// The error kind for running sqllogictest.
#[derive(thiserror::Error, Debug, Clone)]
pub enum TestErrorKind {
    #[error("statement is expected to fail, but actually succeed:\nSQL: {sql}")]
    StatementOk { sql: String },
    #[error("statement failed: {err}\nSQL: {sql}")]
    StatementFail {
        sql: String,
        err: Rc<dyn std::error::Error>,
    },
    #[error(
        "statement is expected to affect {expected} rows, but actually {actual}\n\tSQL: {sql}"
    )]
    StatementResultMismatch {
        sql: String,
        expected: u64,
        actual: String,
    },
    #[error("query failed: {err}\nSQL: {sql}")]
    QueryFail {
        sql: String,
        err: Rc<dyn std::error::Error>,
    },
    #[error("query result mismatch:\nSQL: {sql}\nExpected:\n{expected}\nActual:\n{actual}")]
    QueryResultMismatch {
        sql: String,
        expected: String,
        actual: String,
    },
}

impl TestErrorKind {
    fn at(self, loc: Location) -> TestError {
        TestError { kind: self, loc }
    }
}

/// Sqllogictest runner.
pub struct Runner<D: DB> {
    db: D,
    testdir: Option<TempDir>,
}

impl<D: DB> Runner<D> {
    /// Create a new test runner on the database.
    pub fn new(db: D) -> Self {
        Runner { db, testdir: None }
    }

    /// Replace the pattern `__TEST_DIR__` in SQL with a temporary directory path.
    ///
    /// This feature is useful in those tests where data will be written to local
    /// files, e.g. `COPY`.
    pub fn enable_testdir(&mut self) {
        self.testdir = Some(tempdir().expect("failed to create testdir"));
    }

    /// Run a single record.
    pub fn run(&mut self, record: Record) -> Result<(), TestError> {
        info!("test: {:?}", record);
        match record {
            Record::Statement {
                error,
                sql,
                loc,
                expected_count,
                ..
            } => {
                let sql = self.replace_keywords(sql);
                let ret = self.db.run(&sql);
                match ret {
                    Ok(_) if error => return Err(TestErrorKind::StatementOk { sql }.at(loc)),
                    Ok(count_str) => {
                        if let Some(expected_count) = expected_count {
                            if expected_count.to_string() != count_str {
                                return Err(TestErrorKind::StatementResultMismatch {
                                    sql,
                                    expected: expected_count,
                                    actual: count_str,
                                }
                                .at(loc));
                            }
                        }
                    }
                    Err(e) if !error => {
                        return Err(TestErrorKind::StatementFail {
                            sql,
                            err: Rc::new(e),
                        }
                        .at(loc))
                    }
                    _ => {}
                }
            }
            Record::Query {
                loc,
                sql,
                expected_results,
                sort_mode,
                ..
            } => {
                let sql = self.replace_keywords(sql);
                let output = match self.db.run(&sql) {
                    Ok(output) => output,
                    Err(e) => {
                        return Err(TestErrorKind::QueryFail {
                            sql,
                            err: Rc::new(e),
                        }
                        .at(loc))
                    }
                };
                let mut output = split_lines_and_normalize(&output);
                let mut expected_results = split_lines_and_normalize(&expected_results);
                match sort_mode {
                    SortMode::NoSort => {}
                    SortMode::RowSort => {
                        output.sort_unstable();
                        expected_results.sort_unstable();
                    }
                    SortMode::ValueSort => todo!("value sort"),
                };
                if output != expected_results {
                    return Err(TestErrorKind::QueryResultMismatch {
                        sql,
                        expected: expected_results.join("\n"),
                        actual: output.join("\n"),
                    }
                    .at(loc));
                }
            }
            Record::Sleep { duration, .. } => std::thread::sleep(duration),
            Record::Halt { .. } => {}
            Record::Subtest { .. } => {}
            Record::Include { loc, .. } => {
                unreachable!("include should be rewritten during link: at {}", loc)
            }
        }
        Ok(())
    }

    /// Run multiple records.
    ///
    /// The runner will stop early once a halt record is seen.
    pub fn run_multi(
        &mut self,
        records: impl IntoIterator<Item = Record>,
    ) -> Result<(), TestError> {
        for record in records.into_iter() {
            if let Record::Halt { .. } = record {
                break;
            }
            self.run(record)?;
        }
        Ok(())
    }

    /// Run a sqllogictest script.
    pub fn run_script(&mut self, script: &str) -> Result<(), TestError> {
        let records = parse(script).expect("failed to parse sqllogictest");
        self.run_multi(records)
    }

    /// Run a sqllogictest file.
    pub fn run_file(&mut self, filename: impl AsRef<Path>) -> Result<(), TestError> {
        let records = parse_file(filename).expect("failed to parse sqllogictest");
        self.run_multi(records)
    }

    /// Replace all keywords in the SQL.
    fn replace_keywords(&self, sql: String) -> String {
        if let Some(testdir) = &self.testdir {
            sql.replace("__TEST_DIR__", testdir.path().to_str().unwrap())
        } else {
            sql
        }
    }
}

/// Trim and replace multiple whitespaces with one.
fn normalize_string(s: &str) -> String {
    s.trim().split_ascii_whitespace().join(" ")
}

fn split_lines_and_normalize(s: &str) -> Vec<String> {
    s.split('\n')
        .map(normalize_string)
        .filter(|line| !line.is_empty())
        .collect()
}
