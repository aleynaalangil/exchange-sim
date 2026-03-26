use crate::auth as jwt;
use crate::db;
use crate::error::AppError;
use crate::models::{new_id, LoginRequest, LoginResponse, User, UserRole};
use crate::AppState;
use actix_web::{post, web, HttpRequest, HttpResponse};
use chrono::Utc;
use rust_decimal::Decimal;
use serde::Deserialize;
use tracing::{info, warn};

/// POST /api/v1/auth/login
#[post("/auth/login")]
pub async fn login(
    state: web::Data<AppState>,
    body: web::Json<LoginRequest>,
) -> Result<HttpResponse, AppError> {
    let user = db::get_user_by_username(&state.db, &body.username)
        .await?
        .ok_or_else(|| AppError::Unauthorized("Invalid credentials".into()))?;

    if !user.is_active {
        return Err(AppError::Unauthorized("Account is disabled".into()));
    }

    let valid = bcrypt::verify(&body.password, &user.password_hash)
        .map_err(|e| AppError::Internal(e.to_string()))?;

    if !valid {
        warn!(username = %body.username, "Failed login attempt");
        return Err(AppError::Unauthorized("Invalid credentials".into()));
    }

    let token = jwt::create_token(
        &user.id,
        user.role.as_str(),
        &state.config.jwt_secret,
        state.config.jwt_expiry_hours,
    )?;

    Ok(HttpResponse::Ok().json(LoginResponse {
        token,
        user_id: user.id,
        username: user.username,
        role: user.role,
    }))
}

#[derive(Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub password: String,
}

/// POST /api/v1/auth/register
/// Public endpoint — anyone can create a trader account.
#[post("/auth/register")]
pub async fn register(
    state: web::Data<AppState>,
    body: web::Json<RegisterRequest>,
) -> Result<HttpResponse, AppError> {
    if body.username.trim().is_empty() || body.password.len() < 6 {
        return Err(AppError::BadRequest(
            "username cannot be empty and password must be at least 6 characters".into(),
        ));
    }

    // Hash outside the lock to keep the critical section short
    let password_hash = bcrypt::hash(&body.password, bcrypt::DEFAULT_COST)
        .map_err(|e| AppError::Internal(e.to_string()))?;

    // Lock to serialise check + insert and prevent duplicate usernames
    let _guard = state.user_creation_lock.lock().await;

    if db::get_user_by_username(&state.db, &body.username)
        .await?
        .is_some()
    {
        return Err(AppError::Conflict(format!(
            "Username '{}' is already taken",
            body.username
        )));
    }

    let user = User {
        id: new_id(),
        username: body.username.trim().to_string(),
        password_hash,
        role: UserRole::Trader,
        balance_usdc: Decimal::from(10000u32),
        created_at: Utc::now(),
        is_active: true,
    };

    db::insert_user(&state.db, &user).await?;
    drop(_guard);

    info!(username = %user.username, "New trader registered");

    let token = jwt::create_token(
        &user.id,
        user.role.as_str(),
        &state.config.jwt_secret,
        state.config.jwt_expiry_hours,
    )?;

    Ok(HttpResponse::Created().json(LoginResponse {
        token,
        user_id: user.id,
        username: user.username,
        role: user.role,
    }))
}

/// POST /api/v1/auth/logout
/// Revokes the caller's token by adding its jti to the in-memory blocklist.
#[post("/auth/logout")]
pub async fn logout(
    req: HttpRequest,
    state: web::Data<AppState>,
) -> Result<HttpResponse, AppError> {
    let claims = jwt::extract_claims(&req, &state.config.jwt_secret, &state.token_blocklist)?;
    state.token_blocklist.insert(claims.jti.clone(), claims.exp);
    info!(user_id = %claims.sub, jti = %claims.jti, "Token revoked");
    Ok(HttpResponse::Ok().json(serde_json::json!({ "message": "Logged out" })))
}
