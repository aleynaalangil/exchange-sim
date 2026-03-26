/// JWT-based stateless auth.
/// Tokens encode user_id + role + jti so every request is self-contained
/// and individual tokens can be revoked server-side via the in-memory blocklist.
use crate::error::AppError;
use crate::models::UserRole;
use chrono::{Duration, Utc};
use dashmap::DashMap;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// In-memory revocation list: jti → expiry (Unix seconds).
/// Entries are cleaned up by a background task once expired.
pub type TokenBlocklist = DashMap<String, u64>;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Claims {
    pub sub: String,   // user_id
    pub role: String,  // "admin" | "trader"
    pub exp: u64,      // Unix timestamp (seconds since epoch)
    pub jti: String,   // unique token ID — used for revocation
}

pub fn create_token(
    user_id: &str,
    role: &str,
    secret: &str,
    expiry_hours: u64,
) -> Result<String, AppError> {
    let exp = (Utc::now() + Duration::hours(expiry_hours as i64))
        .timestamp() as u64;

    let claims = Claims {
        sub: user_id.to_string(),
        role: role.to_string(),
        exp,
        jti: Uuid::new_v4().to_string(),
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| AppError::Internal(e.to_string()))
}

pub fn verify_token(token: &str, secret: &str) -> Result<Claims, AppError> {
    let mut validation = Validation::new(Algorithm::HS256);
    validation.validate_exp = true;

    decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .map(|d| d.claims)
    .map_err(|e| AppError::Unauthorized(e.to_string()))
}

/// Returns Unauthorized if the token's jti is in the revocation list.
pub fn check_not_revoked(claims: &Claims, blocklist: &TokenBlocklist) -> Result<(), AppError> {
    if blocklist.contains_key(&claims.jti) {
        return Err(AppError::Unauthorized("Token has been revoked".into()));
    }
    Ok(())
}

/// Middleware helper: extract and verify claims from `Authorization: Bearer <token>`.
/// Also checks the revocation list.
pub fn extract_claims(
    req: &actix_web::HttpRequest,
    secret: &str,
    blocklist: &TokenBlocklist,
) -> Result<Claims, AppError> {
    let header = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Unauthorized("Missing Authorization header".into()))?;

    let token = header
        .strip_prefix("Bearer ")
        .ok_or_else(|| AppError::Unauthorized("Invalid Authorization format".into()))?;

    let claims = verify_token(token, secret)?;
    check_not_revoked(&claims, blocklist)?;
    Ok(claims)
}

/// Assert caller has admin role.
pub fn require_admin(claims: &Claims) -> Result<(), AppError> {
    if claims.role != UserRole::Admin.as_str() {
        return Err(AppError::Forbidden("Admin only".into()));
    }
    Ok(())
}
