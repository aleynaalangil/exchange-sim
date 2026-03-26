use actix_web::HttpResponse;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("ClickHouse error: {0}")]
    Clickhouse(#[from] clickhouse::error::Error),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Unknown symbol: {0}")]
    UnknownSymbol(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl actix_web::ResponseError for AppError {
    fn error_response(&self) -> HttpResponse {
        let body = serde_json::json!({ "error": self.to_string() });
        match self {
            AppError::NotFound(_) => HttpResponse::NotFound().json(body),
            AppError::Unauthorized(_) => HttpResponse::Unauthorized().json(body),
            AppError::Forbidden(_) => HttpResponse::Forbidden().json(body),
            AppError::Conflict(_) => HttpResponse::Conflict().json(body),
            AppError::BadRequest(_) | AppError::UnknownSymbol(_) => {
                HttpResponse::BadRequest().json(body)
            }
            _ => HttpResponse::InternalServerError().json(body),
        }
    }
}
