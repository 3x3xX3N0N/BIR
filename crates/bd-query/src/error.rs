use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum Error {
    #[error("unexpected character {ch:?} at position {pos}")]
    UnexpectedChar { ch: char, pos: usize },

    #[error("unexpected token {found:?} at position {pos}, expected {expected}")]
    UnexpectedToken {
        found: String,
        pos: usize,
        expected: String,
    },

    #[error("unknown field {0:?}")]
    UnknownField(String),

    #[error("unterminated string starting at position {0}")]
    UnterminatedString(usize),

    #[error("unexpected end of query, expected {0}")]
    UnexpectedEof(String),

    #[error("operator {op} is not valid for field {field}")]
    BadOperator { op: String, field: String },

    #[error("query language not implemented yet")]
    NotImplemented,
}
