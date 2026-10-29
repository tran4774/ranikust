//! The crate's error type.

use std::fmt;

/// Lines of the Dockerfile that an instruction or an error covers: 1-based
/// and inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Lines {
    /// First line.
    pub start: usize,
    /// Last line; equal to `start` for a one-line instruction.
    pub end: usize,
}

impl Lines {
    /// A range of lines; `end` is raised to `start` if it is smaller.
    #[must_use]
    pub fn new(start: usize, end: usize) -> Self {
        Self {
            start,
            end: end.max(start),
        }
    }

    /// A single line.
    #[must_use]
    pub fn single(line: usize) -> Self {
        Self::new(line, line)
    }
}

impl fmt::Display for Lines {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(f, "line {}", self.start)
        } else {
            write!(f, "lines {}-{}", self.start, self.end)
        }
    }
}

/// What kind of problem an [`Error`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// The text is not a valid Dockerfile: a parser directive, a line
    /// continuation, a heredoc, a JSON array or the file as a whole.
    Syntax,
    /// An instruction is unknown, or its arguments or flags are invalid.
    Instruction,
    /// Variable expansion failed, for example on `${VAR?message}` or an
    /// unterminated quote.
    Expansion,
    /// The stages do not form a valid graph: an unknown target, a bad stage
    /// reference or a cycle.
    Graph,
}

/// An error from parsing a Dockerfile, expanding its variables or building
/// its stage graph.
///
/// Errors about the text always carry the lines they refer to; only errors
/// about the build as a whole (an unknown `--target`) have none.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct Error {
    kind: ErrorKind,
    lines: Option<Lines>,
    message: String,
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, lines: Option<Lines>, message: impl Into<String>) -> Self {
        Self {
            kind,
            lines,
            message: message.into(),
        }
    }

    pub(crate) fn at(kind: ErrorKind, lines: Lines, message: impl Into<String>) -> Self {
        Self::new(kind, Some(lines), message)
    }

    /// The kind of problem.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The lines of the Dockerfile the error refers to, if any.
    #[must_use]
    pub fn lines(&self) -> Option<Lines> {
        self.lines
    }

    /// The message, without the location.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.lines {
            Some(lines) => write!(f, "Dockerfile {lines}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_includes_the_lines() {
        let e = Error::at(ErrorKind::Syntax, Lines::new(3, 5), "unterminated heredoc");
        assert_eq!(e.to_string(), "Dockerfile lines 3-5: unterminated heredoc");
        let e = Error::at(ErrorKind::Syntax, Lines::single(2), "x");
        assert_eq!(e.to_string(), "Dockerfile line 2: x");
        let e = Error::new(
            ErrorKind::Graph,
            None,
            "target stage \"x\" could not be found",
        );
        assert_eq!(e.to_string(), "target stage \"x\" could not be found");
        assert_eq!(Lines::new(4, 1), Lines::single(4));
    }
}
