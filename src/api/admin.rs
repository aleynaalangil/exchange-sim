use crate::auth::{extract_claims, require_admin};
use crate::db;
use crate::error::AppError;
use crate::models::{new_id, CreateUserRequest, UpdateBalanceRequest, User, UserRole};
use crate::AppState;
use actix_web::{get, patch, post, web, HttpRequest, HttpResponse};
use chrono::Utc;
use rust_decimal::Decimal;
use tracing::info;

/// GET /api/v1/admin/users
#[get("/admin/users")]
pub async fn list_users(
    req: HttpRequest,
    state: web::Data<AppState>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req, &state.config.jwt_secret, &state.token_blocklist)?;
    require_admin(&claims)?;

    let users = db::list_users(&state.db).await?;
    Ok(HttpResponse::Ok().json(users))
}

/// POST /api/v1/admin/users
/// Create a new user (admin or trader).
#[post("/admin/users")]
pub async fn create_user(
    req: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<CreateUserRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req, &state.config.jwt_secret, &state.token_blocklist)?;
    require_admin(&claims)?;

    let role = UserRole::from_str(&body.role)
        .ok_or_else(|| AppError::BadRequest("role must be 'admin' or 'trader'".into()))?;

    // Hash before acquiring the lock to keep the critical section short
    let password_hash = bcrypt::hash(&body.password, bcrypt::DEFAULT_COST)
        .map_err(|e| AppError::Internal(e.to_string()))?;

    // Lock to serialise check + insert
    let _guard = state.user_creation_lock.lock().await;

    if db::get_user_by_username(&state.db, &body.username)
        .await?
        .is_some()
    {
        return Err(AppError::Conflict(format!(
            "Username '{}' already exists",
            body.username
        )));
    }

    let user = User {
        id: new_id(),
        username: body.username.clone(),
        password_hash,
        role,
        balance_usdc: body
            .initial_balance
            .unwrap_or_else(|| Decimal::from(10000u32)),
        created_at: Utc::now(),
        is_active: true,
    };

    db::insert_user(&state.db, &user).await?;
    drop(_guard);

    info!(username = %user.username, role = %user.role, "User created by admin");

    Ok(HttpResponse::Created().json(serde_json::json!({
        "id": user.id,
        "username": user.username,
        "role": user.role,
        "balance_usdc": user.balance_usdc,
        "created_at": user.created_at,
    })))
}

/// PATCH /api/v1/admin/users/{user_id}/balance
#[patch("/admin/users/{user_id}/balance")]
pub async fn update_balance(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
    body: web::Json<UpdateBalanceRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req, &state.config.jwt_secret, &state.token_blocklist)?;
    require_admin(&claims)?;

    let user_id = path.into_inner();
    let mut user = db::get_user_by_id(&state.db, &user_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("User {} not found", user_id)))?;

    if body.balance_usdc < Decimal::ZERO {
        return Err(AppError::BadRequest("balance_usdc must be >= 0".into()));
    }

    user.balance_usdc = body.balance_usdc;
    user.created_at = Utc::now(); // bump version for ReplacingMergeTree
    db::update_user_balance(&state.db, &user).await?;

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "user_id": user.id,
        "balance_usdc": user.balance_usdc,
    })))
}

/// GET /api/v1/admin/orders?limit=100
#[get("/admin/orders")]
pub async fn list_all_orders(
    req: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<LimitQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req, &state.config.jwt_secret, &state.token_blocklist)?;
    require_admin(&claims)?;

    let limit = query.limit.unwrap_or(100).min(1000);
    let orders = db::list_all_orders(&state.db, limit).await?;
    Ok(HttpResponse::Ok().json(orders))
}

/// GET /api/v1/admin/symbols
#[get("/admin/symbols")]
pub async fn list_symbols(
    req: HttpRequest,
    state: web::Data<AppState>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req, &state.config.jwt_secret, &state.token_blocklist)?;
    require_admin(&claims)?;

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "symbols": state.config.symbols,
    })))
}

/// GET /api/v1/admin/prices — live price snapshot for all symbols
#[get("/admin/prices")]
pub async fn live_prices(
    req: HttpRequest,
    state: web::Data<AppState>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req, &state.config.jwt_secret, &state.token_blocklist)?;
    require_admin(&claims)?;

    let snapshot: serde_json::Map<String, serde_json::Value> = state
        .prices
        .iter()
        .map(|entry| (entry.key().clone(), serde_json::json!(*entry.value())))
        .collect();

    Ok(HttpResponse::Ok().json(snapshot))
}

#[derive(serde::Deserialize)]
pub struct LimitQuery {
    pub limit: Option<u32>,
}
