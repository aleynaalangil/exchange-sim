/// Order execution engine.
/// Market orders execute immediately at the live HFT price.
/// Limit orders lock funds and are placed as Pending; the order_book matcher fills them.
use crate::db;
use crate::error::AppError;
use crate::models::{new_id, Order, OrderStatus, OrderType, Position, Side, User};
use crate::order_book::{new_pending, PendingLimitOrder};
use crate::ws_client::PriceCache;
use chrono::Utc;
use clickhouse::Client;
use rust_decimal::prelude::*;
use rust_decimal::Decimal;
use tracing::info;

pub struct Engine {
    pub client: Client,
    pub prices: PriceCache,
    /// Supported trading pairs — loaded from Config.symbols at startup.
    pub symbols: Vec<String>,
}

impl Engine {
    pub fn new(client: Client, prices: PriceCache, symbols: Vec<String>) -> Self {
        Self {
            client,
            prices,
            symbols,
        }
    }

    /// Place a market or limit order for `user` on `raw_symbol` for `amount` units.
    /// Returns `(Order, Option<PendingLimitOrder>)`.
    /// `Some(PendingLimitOrder)` is returned for limit orders and must be added to the
    /// order book by the caller.
    pub async fn place_order(
        &self,
        user: &mut User,
        raw_symbol: &str,
        side: Side,
        amount: Decimal,
        order_type: OrderType,
        limit_price: Option<Decimal>,
    ) -> Result<(Order, Option<PendingLimitOrder>), AppError> {
        let symbol = self
            .normalize_symbol(raw_symbol)
            .ok_or_else(|| AppError::UnknownSymbol(raw_symbol.to_string()))?;

        if amount <= Decimal::ZERO {
            return Err(AppError::BadRequest("amount must be > 0".to_string()));
        }

        match order_type {
            OrderType::Market => self.execute_market(user, symbol, side, amount).await,
            OrderType::Limit => {
                let lp = limit_price.ok_or_else(|| {
                    AppError::BadRequest("limit_price is required for limit orders".into())
                })?;
                if lp <= Decimal::ZERO {
                    return Err(AppError::BadRequest("limit_price must be > 0".into()));
                }
                self.execute_limit(user, symbol, side, amount, lp).await
            }
            OrderType::StopLimit => Err(AppError::BadRequest(
                "stop_limit orders are not yet supported".into(),
            )),
        }
    }

    fn normalize_symbol(&self, raw: &str) -> Option<String> {
        let upper = raw.to_uppercase().replace('-', "/");
        if self.symbols.contains(&upper) {
            Some(upper)
        } else {
            None
        }
    }

    // ── Market order ──────────────────────────────────────────────────────────

    async fn execute_market(
        &self,
        user: &mut User,
        symbol: String,
        side: Side,
        amount: Decimal,
    ) -> Result<(Order, Option<PendingLimitOrder>), AppError> {
        let price_f64 = self
            .prices
            .get(&symbol)
            .map(|p| *p)
            .ok_or_else(|| AppError::Internal(format!("No live price for {}", symbol)))?;

        let price = Decimal::from_f64(price_f64)
            .ok_or_else(|| AppError::Internal(format!("Invalid price value: {}", price_f64)))?
            .round_dp(8);

        let total_usdc = (price * amount).round_dp(8);

        let (status, reject_reason) = match side {
            Side::Buy => {
                self.fill_market_buy(user, &symbol, price, amount, total_usdc)
                    .await?
            }
            Side::Sell => {
                self.fill_market_sell(user, &symbol, amount, total_usdc)
                    .await?
            }
        };

        let now = Utc::now();
        let order = Order {
            id: new_id(),
            user_id: user.id.clone(),
            symbol,
            side,
            order_type: OrderType::Market,
            limit_price: None,
            price,
            amount,
            total_usdc,
            status,
            reject_reason,
            created_at: now,
            updated_at: now,
        };

        db::insert_order(&self.client, &order).await?;
        db::update_user_balance(&self.client, user).await?;

        info!(
            order_id = %order.id,
            user     = %user.username,
            symbol   = %order.symbol,
            side     = %order.side,
            price    = %order.price,
            amount   = %order.amount,
            status   = %order.status,
            "Market order processed"
        );

        Ok((order, None))
    }

    async fn fill_market_buy(
        &self,
        user: &mut User,
        symbol: &str,
        price: Decimal,
        amount: Decimal,
        total_usdc: Decimal,
    ) -> Result<(OrderStatus, Option<String>), AppError> {
        if user.balance_usdc < total_usdc {
            return Ok((
                OrderStatus::Rejected,
                Some(format!(
                    "Insufficient balance: need {} USDC, have {}",
                    total_usdc, user.balance_usdc
                )),
            ));
        }
        user.balance_usdc -= total_usdc;
        pub_update_position_buy(&self.client, user, symbol, price, amount).await?;
        Ok((OrderStatus::Filled, None))
    }

    async fn fill_market_sell(
        &self,
        user: &mut User,
        symbol: &str,
        amount: Decimal,
        total_usdc: Decimal,
    ) -> Result<(OrderStatus, Option<String>), AppError> {
        let positions = db::get_positions_for_user(&self.client, &user.id).await?;
        let pos = positions.iter().find(|p| p.symbol == symbol);
        let held = pos.map(|p| p.quantity).unwrap_or(Decimal::ZERO);

        if held < amount {
            return Ok((
                OrderStatus::Rejected,
                Some(format!(
                    "Insufficient position: need {}, have {} {}",
                    amount, held, symbol
                )),
            ));
        }

        user.balance_usdc += total_usdc;
        pub_update_position_sell(&self.client, symbol, pos, amount).await?;
        Ok((OrderStatus::Filled, None))
    }

    // ── Limit order ───────────────────────────────────────────────────────────

    async fn execute_limit(
        &self,
        user: &mut User,
        symbol: String,
        side: Side,
        amount: Decimal,
        limit_price: Decimal,
    ) -> Result<(Order, Option<PendingLimitOrder>), AppError> {
        let now = Utc::now();

        let (locked_usdc, locked_qty) = match side {
            Side::Buy => {
                let cost = (limit_price * amount).round_dp(8);
                if user.balance_usdc < cost {
                    let order = rejected_order(
                        &user.id,
                        &symbol,
                        side,
                        OrderType::Limit,
                        amount,
                        now,
                        Some(limit_price),
                        format!(
                            "Insufficient balance: need {} USDC, have {}",
                            cost, user.balance_usdc
                        ),
                    );
                    db::insert_order(&self.client, &order).await?;
                    return Ok((order, None));
                }
                user.balance_usdc -= cost;
                (cost, Decimal::ZERO)
            }
            Side::Sell => {
                let positions = db::get_positions_for_user(&self.client, &user.id).await?;
                let pos = positions.iter().find(|p| p.symbol == symbol);
                let held = pos.map(|p| p.quantity).unwrap_or(Decimal::ZERO);

                if held < amount {
                    let order = rejected_order(
                        &user.id,
                        &symbol,
                        side,
                        OrderType::Limit,
                        amount,
                        now,
                        Some(limit_price),
                        format!(
                            "Insufficient position: need {}, have {} {}",
                            amount, held, symbol
                        ),
                    );
                    db::insert_order(&self.client, &order).await?;
                    return Ok((order, None));
                }
                // Deduct position immediately (locked)
                pub_update_position_sell(&self.client, &symbol, pos, amount).await?;
                (Decimal::ZERO, amount)
            }
        };

        let order = Order {
            id: new_id(),
            user_id: user.id.clone(),
            symbol: symbol.clone(),
            side,
            order_type: OrderType::Limit,
            limit_price: Some(limit_price),
            price: Decimal::ZERO,
            amount,
            total_usdc: Decimal::ZERO,
            status: OrderStatus::Pending,
            reject_reason: None,
            created_at: now,
            updated_at: now,
        };

        db::insert_order(&self.client, &order).await?;
        db::update_user_balance(&self.client, user).await?;

        let pending = new_pending(&order, locked_usdc, locked_qty);

        info!(
            order_id    = %order.id,
            user        = %user.username,
            symbol      = %order.symbol,
            side        = %order.side,
            limit_price = %limit_price,
            amount      = %order.amount,
            "Limit order queued"
        );

        Ok((order, Some(pending)))
    }
}

// ── Shared helpers (pub(crate) so order_book can reuse them) ──────────────────

pub(crate) async fn pub_update_position_buy(
    client: &Client,
    user: &User,
    symbol: &str,
    price: Decimal,
    amount: Decimal,
) -> Result<(), AppError> {
    let positions = db::get_positions_for_user(client, &user.id).await?;
    let existing = positions.iter().find(|p| p.symbol == symbol);

    let new_pos = match existing {
        None => Position {
            user_id: user.id.clone(),
            symbol: symbol.to_string(),
            quantity: amount,
            avg_buy_price: price,
            updated_at: Utc::now(),
        },
        Some(pos) => {
            let total_cost = pos.avg_buy_price * pos.quantity + price * amount;
            let new_qty = pos.quantity + amount;
            Position {
                user_id: user.id.clone(),
                symbol: symbol.to_string(),
                quantity: new_qty,
                avg_buy_price: (total_cost / new_qty).round_dp(8),
                updated_at: Utc::now(),
            }
        }
    };

    db::upsert_position(client, &new_pos).await
}

pub(crate) async fn pub_update_position_sell(
    client: &Client,
    symbol: &str,
    existing: Option<&Position>,
    amount: Decimal,
) -> Result<(), AppError> {
    let pos = existing.expect("sell guard already confirmed position exists");
    let new_qty = (pos.quantity - amount).max(Decimal::ZERO);

    let new_pos = Position {
        user_id: pos.user_id.clone(),
        symbol: symbol.to_string(),
        quantity: new_qty,
        avg_buy_price: if new_qty.is_zero() {
            Decimal::ZERO
        } else {
            pos.avg_buy_price
        },
        updated_at: Utc::now(),
    };

    db::upsert_position(client, &new_pos).await
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn rejected_order(
    user_id: &str,
    symbol: &str,
    side: Side,
    order_type: OrderType,
    amount: Decimal,
    now: chrono::DateTime<Utc>,
    limit_price: Option<Decimal>,
    reason: String,
) -> Order {
    Order {
        id: new_id(),
        user_id: user_id.to_string(),
        symbol: symbol.to_string(),
        side,
        order_type,
        limit_price,
        price: Decimal::ZERO,
        amount,
        total_usdc: Decimal::ZERO,
        status: OrderStatus::Rejected,
        reject_reason: Some(reason),
        created_at: now,
        updated_at: now,
    }
}
