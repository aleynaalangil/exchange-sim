//! In-memory limit order book and background matcher.
//!
//! When a limit order is placed, funds are locked immediately:
//!   - Buy limit:  USDC deducted from user balance (locked_usdc = limit_price × amount).
//!   - Sell limit: asset quantity deducted from position (locked_qty = amount).
//!
//! A background task checks every 500ms whether any pending orders can be
//! filled at the current market price:
//!   - Buy fills when market_price ≤ limit_price.
//!   - Sell fills when market_price ≥ limit_price.
//!
//! On fill:   position / balance is updated; excess locked USDC (buy) is refunded.
//! On cancel: locked funds are returned before the order is marked Canceled.

use crate::db;
use crate::error::AppError;
use crate::models::{Order, Position, Side};
use crate::ws_client::PriceCache;
use chrono::Utc;
use clickhouse::Client;
use dashmap::DashMap;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{interval, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

// ── PendingLimitOrder ─────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct PendingLimitOrder {
    pub order_id: String,
    pub user_id: String,
    pub side: Side,
    pub symbol: String,
    pub amount: Decimal,
    pub limit_price: Decimal,
    pub created_at: chrono::DateTime<Utc>,
    /// For buy orders: limit_price × amount was deducted from balance.
    pub locked_usdc: Decimal,
    /// For sell orders: the asset quantity was deducted from position.
    pub locked_qty: Decimal,
}

// ── Per-user serialization locks ──────────────────────────────────────────────

/// One tokio Mutex per user_id — prevents concurrent order mutations for the
/// same user (place + place, place + cancel, fill + cancel, etc.).
pub type UserLocks = Arc<DashMap<String, Arc<Mutex<()>>>>;

pub fn new_user_locks() -> UserLocks {
    Arc::new(DashMap::new())
}

/// Gets or lazily creates the per-user Mutex. Clone the Arc *before* awaiting
/// so the DashMap shard lock is not held across an await point.
pub fn get_user_lock(locks: &UserLocks, user_id: &str) -> Arc<Mutex<()>> {
    let entry = locks
        .entry(user_id.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())));
    Arc::clone(&*entry)
}

// ── OrderBook ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct OrderBook {
    inner: Arc<RwLock<Vec<PendingLimitOrder>>>,
}

impl OrderBook {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub async fn add(&self, order: PendingLimitOrder) {
        self.inner.write().await.push(order);
    }

    /// Removes and returns the order if found (needed by cancel to reclaim locked funds).
    pub async fn remove(&self, order_id: &str) -> Option<PendingLimitOrder> {
        let mut book = self.inner.write().await;
        book.iter()
            .position(|o| o.order_id == order_id)
            .map(|pos| book.remove(pos))
    }

    /// Atomically drains all orders that can be filled at the current market prices.
    /// Returns (PendingLimitOrder, exec_price) pairs.
    async fn drain_fillable(&self, prices: &PriceCache) -> Vec<(PendingLimitOrder, Decimal)> {
        let mut book = self.inner.write().await;
        let mut fillable = Vec::new();
        book.retain(|order| {
            let raw_price = match prices.get(&order.symbol) {
                Some(p) => *p,
                None => return true, // no price yet — keep
            };
            let market_price = match Decimal::from_f64(raw_price) {
                Some(p) => p,
                None => return true,
            };
            let can_fill = match order.side {
                Side::Buy => market_price <= order.limit_price,
                Side::Sell => market_price >= order.limit_price,
            };
            if can_fill {
                fillable.push((order.clone(), market_price));
                false // remove from book
            } else {
                true // keep
            }
        });
        fillable
    }

    /// Spawns the background matching loop. Call once from main().
    pub async fn run_matcher(
        self,
        prices: PriceCache,
        db: Client,
        user_locks: UserLocks,
        cancel: CancellationToken,
    ) {
        let mut ticker = interval(Duration::from_millis(500));
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    info!("Order book matcher shutting down");
                    return;
                }
                _ = ticker.tick() => {
                    let fillable = self.drain_fillable(&prices).await;
                    for (pending, exec_price) in fillable {
                        let lock = get_user_lock(&user_locks, &pending.user_id);
                        let _guard = lock.lock().await;

                        if let Err(e) = fill_order(&db, &pending, exec_price).await {
                            error!(
                                order_id = %pending.order_id,
                                user_id  = %pending.user_id,
                                error    = %e,
                                "Failed to fill limit order — re-queuing"
                            );
                            // Re-add so it can be retried on the next tick.
                            self.inner.write().await.push(pending);
                        }
                    }
                }
            }
        }
    }
}

// ── Fill logic ────────────────────────────────────────────────────────────────

async fn fill_order(
    db: &Client,
    pending: &PendingLimitOrder,
    exec_price: Decimal,
) -> Result<(), AppError> {
    let total_usdc = (exec_price * pending.amount).round_dp(8);
    let now = Utc::now();

    // Build a minimal Order shell to pass to update_order_fill (needs all fields).
    let shell = Order {
        id: pending.order_id.clone(),
        user_id: pending.user_id.clone(),
        symbol: pending.symbol.clone(),
        side: pending.side,
        order_type: crate::models::OrderType::Limit,
        limit_price: Some(pending.limit_price),
        price: Decimal::ZERO,
        amount: pending.amount,
        total_usdc: Decimal::ZERO,
        status: crate::models::OrderStatus::Pending,
        reject_reason: None,
        created_at: pending.created_at,
        updated_at: now,
    };

    db::update_order_fill(db, &shell, exec_price, total_usdc).await?;

    match pending.side {
        Side::Buy => {
            // Funds were already locked. Credit the position.
            let existing = db::get_positions_for_user(db, &pending.user_id).await?;
            let pos = existing.iter().find(|p| p.symbol == pending.symbol);

            let new_pos = match pos {
                None => Position {
                    user_id: pending.user_id.clone(),
                    symbol: pending.symbol.clone(),
                    quantity: pending.amount,
                    avg_buy_price: exec_price,
                    updated_at: now,
                },
                Some(p) => {
                    let total_cost = p.avg_buy_price * p.quantity + exec_price * pending.amount;
                    let new_qty = p.quantity + pending.amount;
                    Position {
                        user_id: pending.user_id.clone(),
                        symbol: pending.symbol.clone(),
                        quantity: new_qty,
                        avg_buy_price: (total_cost / new_qty).round_dp(8),
                        updated_at: now,
                    }
                }
            };
            db::upsert_position(db, &new_pos).await?;

            // Refund any excess locked USDC (locked at limit_price, filled at exec_price ≤ limit_price).
            let actual_cost = total_usdc;
            let refund = (pending.locked_usdc - actual_cost).max(Decimal::ZERO);
            if refund > Decimal::ZERO {
                let mut user =
                    db::get_user_by_id(db, &pending.user_id)
                        .await?
                        .ok_or_else(|| {
                            AppError::NotFound("User not found during fill refund".into())
                        })?;
                user.balance_usdc += refund;
                user.created_at = now;
                db::update_user_balance(db, &user).await?;
            }
        }
        Side::Sell => {
            // Position was already locked. Credit USDC.
            let mut user = db::get_user_by_id(db, &pending.user_id)
                .await?
                .ok_or_else(|| AppError::NotFound("User not found during fill".into()))?;
            user.balance_usdc += total_usdc;
            user.created_at = now;
            db::update_user_balance(db, &user).await?;
        }
    }

    info!(
        order_id  = %pending.order_id,
        user_id   = %pending.user_id,
        symbol    = %pending.symbol,
        side      = %pending.side,
        exec_price = %exec_price,
        total_usdc = %total_usdc,
        "Limit order filled"
    );

    Ok(())
}

/// Build a `PendingLimitOrder` id for a freshly created limit order that has
/// no order_id yet. Pass this along with the `Order` returned by the engine.
pub fn new_pending(order: &Order, locked_usdc: Decimal, locked_qty: Decimal) -> PendingLimitOrder {
    PendingLimitOrder {
        order_id: order.id.clone(),
        user_id: order.user_id.clone(),
        side: order.side,
        symbol: order.symbol.clone(),
        amount: order.amount,
        limit_price: order.limit_price.unwrap_or(Decimal::ZERO),
        created_at: order.created_at,
        locked_usdc,
        locked_qty,
    }
}
