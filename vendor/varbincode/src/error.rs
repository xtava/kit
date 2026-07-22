use serde::{de, ser};
use std::fmt::{self, Display};

#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    Message(String),
    Io(String),
    SequenceMustHaveLength,
    LebOverflow,
    DeserializeAnyNotSupported,
    DeserializeIdentifierNotSupported,
    DeserializeIgnoredAnyNotSupported,
    InvalidBoolEncoding(u8),
    InvalidCharEncoding(u32),
    InvalidUtf8Encoding(std::str::Utf8Error),
    InvalidTagEncoding(usize),
    NumberOutOfRange,
    LimitExceeded { kind: &'static str, limit: usize },
    AllocationFailed { requested: usize },
}

pub type Result<T> = std::result::Result<T, Error>;

impl ser::Error for Error {
    fn custom<T: Display>(msg: T) -> Self {
        Error::Message(msg.to_string())
    }
}

impl de::Error for Error {
    fn custom<T: Display>(msg: T) -> Self {
        Error::Message(msg.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Error {
        Error::Io(format!("{}", err))
    }
}

impl From<leb128::read::Error> for Error {
    fn from(err: leb128::read::Error) -> Error {
        match err {
            leb128::read::Error::IoError(err) => Error::Io(format!("{}", err)),
            leb128::read::Error::Overflow => Error::LebOverflow,
        }
    }
}

impl Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Message(message) | Error::Io(message) => formatter.write_str(message),
            Error::SequenceMustHaveLength => formatter.write_str("SequenceMustHaveLength"),
            Error::LebOverflow => formatter.write_str("LEB128 Overflow"),
            Error::DeserializeAnyNotSupported => formatter.write_str("DeserializeAnyNotSupported"),
            Error::DeserializeIdentifierNotSupported => {
                formatter.write_str("DeserializeIdentifierNotSupported")
            }
            Error::DeserializeIgnoredAnyNotSupported => {
                formatter.write_str("DeserializeIgnoredAnyNotSupported")
            }
            Error::InvalidBoolEncoding(value) => {
                write!(formatter, "Invalid Bool Encoding: {value}")
            }
            Error::InvalidCharEncoding(value) => {
                write!(formatter, "Invalid char encoding: {value}")
            }
            Error::InvalidUtf8Encoding(error) => write!(formatter, "InvalidUtf8Encoding: {error}"),
            Error::InvalidTagEncoding(tag) => write!(formatter, "InvalidTagEncoding: {tag}"),
            Error::NumberOutOfRange => formatter.write_str("NumberOutOfRange"),
            Error::LimitExceeded { kind, limit } => {
                write!(formatter, "Decode limit exceeded for {kind}: {limit}")
            }
            Error::AllocationFailed { requested } => {
                write!(formatter, "Allocation failed for {requested} bytes")
            }
        }
    }
}

impl std::error::Error for Error {}
