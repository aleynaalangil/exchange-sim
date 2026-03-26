use crate::auth::extract_claims;
use crate::db;
use crate::error::AppError;
use crate::models::{
    AccountResponse, OrderStatus, OrderType, PnlByOrderType, PnlStats, RealizedPnlDetail,
    UnrealizedPositionPnl,
};
use crate::AppState;
use actix_web::{get, web, HttpRequest, HttpResponse};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;

/// GET /api/v1/account
/// Returns the authenticated trader's balance and open positions.
#[get("/account")]
pub async fn get_account(
    req: HttpRequest,
    state: web::Data<AppState>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req, &state.config.jwt_secret, &state.token_blocklist)?;

    let user = db::get_user_by_id(&state.db, &claims.sub)
        .await?
        .ok_or_else(|| AppError::NotFound("User not found".into()))?;

    let positions = db::get_positions_for_user(&state.db, &user.id).await?;

    Ok(HttpResponse::Ok().json(AccountResponse {
        user_id: user.id,
        username: user.username,
        role: user.role,
        balance_usdc: user.balance_usdc,
        positions,
    }))
}

/// GET /api/v1/account/pnl
/// Returns realized and unrealized P&L for the authenticated user.
///
/// Realized PnL per symbol:
///   avg_buy_price = Σ(buy.price × buy.amount) / Σ(buy.amount)   [all filled buys]
///   pnl per sell  = (sell.price − avg_buy_price) × sell.amount
///
/// Unrealized PnL per symbol (open positions):
///   pnl = (current_price − position.avg_buy_price) × position.quantity
#[get("/account/pnl")]
pub async fn get_pnl(
    req: HttpRequest,
    state: web::Data<AppState>,
) -> Result<HttpResponse, AppError> {
    let claims = extract_claims(&req, &state.config.jwt_secret, &state.token_blocklist)?;
    let user_id = &claims.sub;

    // Fetch up to 10 000 orders (sufficient for a simulator)
    let orders = db::list_orders_for_user(&state.db, user_id, 10_000).await?;
    let positions = db::get_positions_for_user(&state.db, user_id).await?;

    // ── Realized PnL ────────────────────────────────────────────────────────

    // Per-symbol: accumulate total cost and total quantity for filled buys
    let mut buy_cost: HashMap<String, Decimal> = HashMap::new();
    let mut buy_qty: HashMap<String, Decimal> = HashMap::new();

    for o in orders
        .iter()
        .filter(|o| o.status == OrderStatus::Filled && o.side == crate::models::Side::Buy)
    {
        *buy_cost.entry(o.symbol.clone()).or_insert(Decimal::ZERO) += o.price * o.amount;
        *buy_qty.entry(o.symbol.clone()).or_insert(Decimal::ZERO) += o.amount;
    }

    // avg_buy_price per symbol
    let avg_buy: HashMap<String, Decimal> = buy_cost
        .iter()
        .filter_map(|(sym, cost)| {
            let qty = buy_qty.get(sym)?;
            if qty.is_zero() {
                return None;
            }
            Some((sym.clone(), (*cost / *qty).round_dp(8)))
        })
        .collect();

    // Accumulate realized PnL per (symbol, order_type)
    let mut realized_map: HashMap<(String, OrderType), Decimal> = HashMap::new();

    for o in orders
        .iter()
        .filter(|o| o.status == OrderStatus::Filled && o.side == crate::models::Side::Sell)
    {
        let avg = avg_buy.get(&o.symbol).copied().unwrap_or(Decimal::ZERO);
        let pnl = (o.price - avg) * o.amount;
        *realized_map
            .entry((o.symbol.clone(), o.order_type))
            .or_insert(Decimal::ZERO) += pnl;
    }

    // Flatten into details vec and roll up by symbol / order_type
    let mut realized_by_symbol: HashMap<String, Decimal> = HashMap::new();
    let mut by_market = Decimal::ZERO;
    let mut by_limit = Decimal::ZERO;
    let mut by_stop_limit = Decimal::ZERO;
    let mut realized_details: Vec<RealizedPnlDetail> = Vec::new();

    for ((symbol, order_type), pnl) in &realized_map {
        realized_details.push(RealizedPnlDetail {
            symbol: symbol.clone(),
            order_type: *order_type,
            pnl: *pnl,
        });
        *realized_by_symbol
            .entry(symbol.clone())
            .or_insert(Decimal::ZERO) += pnl;
        match order_type {
            OrderType::Market => by_market += pnl,
            OrderType::Limit => by_limit += pnl,
            OrderType::StopLimit => by_stop_limit += pnl,
        }
    }

    // Sort details for stable output
    realized_details.sort_by(|a, b| {
        a.symbol
            .cmp(&b.symbol)
            .then(a.order_type.as_str().cmp(b.order_type.as_str()))
    });

    let realized_total: Decimal = realized_by_symbol.values().sum();

    // ── Unrealized PnL ──────────────────────────────────────────────────────

    let mut unrealized_by_symbol: HashMap<String, UnrealizedPositionPnl> = HashMap::new();
    let mut unrealized_total = Decimal::ZERO;

    for pos in &positions {
        if pos.quantity.is_zero() {
            continue;
        }
        let current_price = state
            .prices
            .get(&pos.symbol)
            .and_then(|p| Decimal::from_f64(*p))
            .unwrap_or(Decimal::ZERO)
            .round_dp(8);

        let pnl = (current_price - pos.avg_buy_price) * pos.quantity;
        unrealized_total += pnl;

        unrealized_by_symbol.insert(
            pos.symbol.clone(),
            UnrealizedPositionPnl {
                pnl,
                quantity: pos.quantity,
                avg_buy_price: pos.avg_buy_price,
                current_price,
            },
        );
    }

    Ok(HttpResponse::Ok().json(PnlStats {
        realized_total,
        realized_by_symbol,
        realized_by_order_type: PnlByOrderType {
            market: by_market,
            limit: by_limit,
            stop_limit: by_stop_limit,
        },
        realized_details,
        unrealized_total,
        unrealized_by_symbol,
    }))
}
