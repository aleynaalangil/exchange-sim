use crate::auth::extract_claims;
use crate::db;
use crate::engine::Engine;
use crate::error::AppError;
use crate::models::{OrderStatus, PlaceOrderRequest, Position};
use crate::order_book::get_user_lock;
use crate::AppState;
use actix_web::{delete, get, post, web, HttpRequest, HttpResponse};
use chrono::Utc;
use rust_decimal::Decimal;

/// POST /api/v1/orders
/// Places a market or limit order at the live HFT price.
#[post("/orders")]
pub async fn place_order(
    req: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<PlaceOrderRequest>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req, &state.config.jwt_secret, &state.token_blocklist)?;

    // Acquire per-user lock before reading user to prevent concurrent orders
    // from the same user both passing the balance check.
    let user_lock = get_user_lock(&state.user_locks, &claims.sub);
    let _order_guard = user_lock.lock().await;

    let mut user = db::get_user_by_id(&state.db, &claims.sub)
        .await?
        .ok_or_else(|| AppError::NotFound("User not found".into()))?;

    let engine = Engine::new(
        state.db.clone(),
        state.prices.clone(),
        state.config.symbols.clone(),
    );

    let (order, maybe_pending) = engine
        .place_order(
            &mut user,
            &body.symbol,
            body.side,
            body.amount,
            body.order_type,
            body.limit_price,
        )
        .await?;

    // Register pending limit orders in the in-memory book
    if let Some(pending) = maybe_pending {
        state.order_book.add(pending).await;
    }

    drop(_order_guard);

    let status_code = if order.status == OrderStatus::Filled {
        actix_web::http::StatusCode::CREATED
    } else {
        actix_web::http::StatusCode::OK
    };

    Ok(HttpResponse::build(status_code).json(order))
}

/// GET /api/v1/orders?limit=50
/// Returns the authenticated user's order history.
#[get("/orders")]
pub async fn list_orders(
    req: HttpRequest,
    state: web::Data<AppState>,
    query: web::Query<LimitQuery>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req, &state.config.jwt_secret, &state.token_blocklist)?;
    let limit = query.limit.unwrap_or(50).min(500);

    let orders = db::list_orders_for_user(&state.db, &claims.sub, limit).await?;
    Ok(HttpResponse::Ok().json(orders))
}

/// DELETE /api/v1/orders/{id}
/// Cancels a pending limit order and returns locked funds to the user.
#[delete("/orders/{order_id}")]
pub async fn cancel_order(
    req: HttpRequest,
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req, &state.config.jwt_secret, &state.token_blocklist)?;
    let order_id = path.into_inner();

    // Acquire per-user lock so cancel and the background matcher don't race
    let user_lock = get_user_lock(&state.user_locks, &claims.sub);
    let _order_guard = user_lock.lock().await;

    // Remove from in-memory book first
    let maybe_pending = state.order_book.remove(&order_id).await;

    if maybe_pending.is_none() {
        // Not in book — check DB to give a descriptive error
        let order = db::get_order_by_id(&state.db, &order_id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Order {} not found", order_id)))?;

        if order.user_id != claims.sub {
            return Err(AppError::Forbidden("Order does not belong to you".into()));
        }
        return Err(AppError::BadRequest(format!(
            "Order cannot be canceled: status is {}",
            order.status
        )));
    }

    let pending = maybe_pending.unwrap();

    if pending.user_id != claims.sub {
        // Wrong user — put it back and reject
        state.order_book.add(pending).await;
        return Err(AppError::Forbidden("Order does not belong to you".into()));
    }

    // Fetch the full order record (needed by update_order_status)
    let order = db::get_order_by_id(&state.db, &order_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Order {} not found in DB", order_id)))?;

    // Return locked funds
    let mut user = db::get_user_by_id(&state.db, &claims.sub)
        .await?
        .ok_or_else(|| AppError::NotFound("User not found".into()))?;

    let now = Utc::now();

    match pending.side {
        crate::models::Side::Buy => {
            // Refund locked USDC
            user.balance_usdc += pending.locked_usdc;
            user.created_at = now;
            db::update_user_balance(&state.db, &user).await?;
        }
        crate::models::Side::Sell => {
            // Refund locked position quantity
            let positions = db::get_positions_for_user(&state.db, &user.id).await?;
            let existing = positions.iter().find(|p| p.symbol == pending.symbol);
            let existing_qty = existing.map(|p| p.quantity).unwrap_or(Decimal::ZERO);
            let existing_avg = existing.map(|p| p.avg_buy_price).unwrap_or(Decimal::ZERO);

            let new_pos = Position {
                user_id: user.id.clone(),
                symbol: pending.symbol.clone(),
                quantity: existing_qty + pending.locked_qty,
                avg_buy_price: existing_avg,
                updated_at: now,
            };
            db::upsert_position(&state.db, &new_pos).await?;
        }
    }

    db::update_order_status(&state.db, &order, OrderStatus::Canceled).await?;

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "order_id": order_id,
        "status": "canceled",
    })))
}

#[derive(serde::Deserialize)]
pub struct LimitQuery {
    pub limit: Option<u32>,
}
