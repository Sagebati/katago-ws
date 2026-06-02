//! Application-wide error type and its HTTP representation.

// Bring the re-exported aide into scope as `aide`: the `OperationIo` derive
// (and its `output_with`) expand to relative `aide::OperationInput`/`Output`
// paths, which resolve through this — so no direct `aide` dependency is needed.
use muxa::aide::{self, OperationIo};
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use schemars::JsonSchema;
use serde::Serialize;

/// Errors surfaced by the analysis service.
///
/// For OpenAPI, every fallible handler documents its error response as
/// [`ErrorBody`] via the `OperationIo` derive's `output_with` — so the schema
/// is derived from `ErrorBody`'s `JsonSchema`, with no hand-written
/// `OperationOutput` impl.
#[derive(Debug, thiserror::Error, OperationIo)]
#[aide(output_with = "axum::Json<ErrorBody>")]
pub enum AppError {
    /// SGF could not be parsed or replayed.
    #[error("invalid SGF: {0}")]
    Sgf(String),
    /// Requested resource does not exist.
    #[error("not found")]
    NotFound,
    /// ONNX inference failed.
    #[error("inference error: {0}")]
    Inference(String),
    /// Model/encoder could not be constructed.
    #[error("model load error: {0}")]
    ModelLoad(String),
    /// Database access failed.
    #[error("database error: {0}")]
    Db(String),
    /// Message-queue operation failed.
    #[error("queue error: {0}")]
    Queue(String),
    /// Catch-all internal error.
    #[error("internal error: {0}")]
    Internal(String),
}

/// Convenience alias used throughout the crate.
pub type AppResult<T> = Result<T, AppError>;

/// Lets `AppError` be the error type of a `diesel_async` transaction (whose
/// bound requires `From<diesel::result::Error>` for transaction-management
/// failures like a failed COMMIT).
impl From<diesel::result::Error> for AppError {
    fn from(err: diesel::result::Error) -> Self {
        AppError::Db(err.to_string())
    }
}

/// The JSON body returned for any [`AppError`]: `{ "error": "<message>" }`.
/// Also the documented error schema for fallible endpoints.
#[derive(Serialize, JsonSchema)]
pub struct ErrorBody {
    /// Human-readable error message.
    error: String,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match &self {
            AppError::Sgf(_) => StatusCode::BAD_REQUEST,
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::Inference(_)
            | AppError::ModelLoad(_)
            | AppError::Db(_)
            | AppError::Queue(_)
            | AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (
            status,
            Json(ErrorBody {
                error: self.to_string(),
            }),
        )
            .into_response()
    }
}
