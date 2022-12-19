use actix_web::error::BlockingError;
use actix_web::ResponseError;
use serde::{Deserialize, Serialize};
use std::fmt::Formatter;

#[macro_export]
macro_rules! ierror {
    ($f: literal) => {
        ErrorCode::InternalError(format!($f))
    };
    ($f: literal, $($x:expr),*) => {
        ErrorCode::InternalError(format!($f, $($x),*))
    };
}

#[macro_export]
macro_rules! serror {
    ($f: literal) => {
        ErrorCode::StateError(format!($f))
    };
    ($f: literal, $($x:expr),*) => {
        ErrorCode::StateError(format!($f, $($x),*))
    };
}

#[macro_export]
macro_rules! aerror {
    ($f: literal) => {
        ErrorCode::ArgumentError(format!($f))
    };
    ($f: literal, $($x:expr),*) => {
        ErrorCode::ArgumentError(format!($f, $($x),*))
    };
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum ErrorCode {
    Ok,
    InternalError(String),
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ErrorCode::Ok => write!(f, "Ok"),
            ErrorCode::InternalError(msg) => write!(f, "Internal error. Reason: {}", msg),
        }
    }
}

impl ResponseError for ErrorCode {}
impl std::error::Error for ErrorCode {}

impl From<anyhow::Error> for ErrorCode {
    fn from(e: anyhow::Error) -> Self {
        Self::InternalError(format!("Internal error {}", e))
    }
}

impl From<BlockingError> for ErrorCode {
    fn from(e: BlockingError) -> Self {
        Self::InternalError(format!("Blocking read error {}", e))
    }
}

impl From<std::io::Error> for ErrorCode {
    fn from(e: std::io::Error) -> Self {
        Self::InternalError(format!("IO error {}", e))
    }
}
