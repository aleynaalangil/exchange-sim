use crate::error::AppError;
use crate::models::{Order, OrderStatus, OrderType, Position, Side, User, UserRole};
use chrono::{DateTime, Utc};
use clickhouse::{Client, Row};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tracing::{info, warn};

// ── Client factory ────────────────────────────────────────────────────────────

pub fn make_client(url: &str, user: &str, password: &str, db: &str) -> Client {
    Client::default()
        .with_url(url)
        .with_user(user)
        .with_password(password)
        .with_database(db)
}

// ── Connectivity check ────────────────────────────────────────────────────────

pub async fn check_ready(client: &Client) -> Result<(), AppError> {
    client
        .query("SELECT 1")
        .fetch_one::<u8>()
        .await
        .map_err(AppError::Clickhouse)?;
    info!("ClickHouse connection verified");
    Ok(())
}

// ── ClickHouse row types ──────────────────────────────────────────────────────

#[derive(Row, Deserialize, Serialize)]
struct UserRow {
    id: String,
    username: String,
    password_hash: String,
    role: String,
    balance_usdc: String,
    created_at: String,
    is_active: u8,
}

impl UserRow {
    fn into_model(self) -> User {
        let balance_usdc = Decimal::from_str(&self.balance_usdc).unwrap_or_else(|_| {
            warn!(
                user_id = %self.id,
                raw = %self.balance_usdc,
                "Failed to parse balance_usdc from DB, defaulting to 0"
            );
            Decimal::ZERO
        });
        let role = UserRole::from_str(&self.role).unwrap_or_else(|| {
            warn!(
                user_id = %self.id,
                raw = %self.role,
                "Unknown role in DB, defaulting to Trader"
            );
            UserRole::Trader
        });
        User {
            id: self.id,
            username: self.username,
            password_hash: self.password_hash,
            role,
            balance_usdc,
            created_at: self
                .created_at
                .parse::<DateTime<Utc>>()
                .unwrap_or_else(|_| Utc::now()),
            is_active: self.is_active != 0,
        }
    }
}

#[derive(Row, Deserialize, Serialize)]
struct OrderRow {
    id: String,
    user_id: String,
    symbol: String,
    side: String,
    order_type: String,
    price: String,
    amount: String,
    total_usdc: String,
    limit_price: String,
    status: String,
    reject_reason: String,
    realized_pnl: String,
    created_at: String,
    updated_at: String,
}

impl OrderRow {
    fn into_model(self) -> Order {
        let price = Decimal::from_str(&self.price).unwrap_or_else(|_| {
            warn!(order_id = %self.id, raw = %self.price, "Failed to parse price from DB");
            Decimal::ZERO
        });
        let amount = Decimal::from_str(&self.amount).unwrap_or_else(|_| {
            warn!(order_id = %self.id, raw = %self.amount, "Failed to parse amount from DB");
            Decimal::ZERO
        });
        let total_usdc = Decimal::from_str(&self.total_usdc).unwrap_or_else(|_| {
            warn!(order_id = %self.id, raw = %self.total_usdc, "Failed to parse total_usdc from DB");
            Decimal::ZERO
        });
        let limit_price = if self.limit_price.is_empty() {
            None
        } else {
            Decimal::from_str(&self.limit_price).ok()
        };
        let side = Side::from_str(&self.side).unwrap_or_else(|| {
            warn!(order_id = %self.id, raw = %self.side, "Unknown side in DB, defaulting to Buy");
            Side::Buy
        });
        let order_type = OrderType::from_str(&self.order_type).unwrap_or_else(|| {
            warn!(order_id = %self.id, raw = %self.order_type, "Unknown order_type, defaulting to Market");
            OrderType::Market
        });
        let status = OrderStatus::from_str(&self.status).unwrap_or_else(|| {
            warn!(order_id = %self.id, raw = %self.status, "Unknown status in DB, defaulting to Rejected");
            OrderStatus::Rejected
        });
        Order {
            id: self.id,
            user_id: self.user_id,
            symbol: self.symbol,
            side,
            order_type,
            limit_price,
            price,
            amount,
            total_usdc,
            status,
            reject_reason: if self.reject_reason.is_empty() {
                None
            } else {
                Some(self.reject_reason)
            },
            created_at: self
                .created_at
                .parse::<DateTime<Utc>>()
                .unwrap_or_else(|_| Utc::now()),
            updated_at: self
                .updated_at
                .parse::<DateTime<Utc>>()
                .unwrap_or_else(|_| Utc::now()),
        }
    }
}

#[derive(Row, Deserialize, Serialize)]
struct PositionRow {
    user_id: String,
    symbol: String,
    quantity: String,
    avg_buy_price: String,
    updated_at: String,
}

impl PositionRow {
    fn into_model(self) -> Position {
        let quantity = Decimal::from_str(&self.quantity).unwrap_or_else(|_| {
            warn!(user_id = %self.user_id, symbol = %self.symbol, "Failed to parse quantity from DB");
            Decimal::ZERO
        });
        let avg_buy_price = Decimal::from_str(&self.avg_buy_price).unwrap_or_else(|_| {
            warn!(user_id = %self.user_id, symbol = %self.symbol, "Failed to parse avg_buy_price from DB");
            Decimal::ZERO
        });
        Position {
            user_id: self.user_id,
            symbol: self.symbol,
            quantity,
            avg_buy_price,
            updated_at: self
                .updated_at
                .parse::<DateTime<Utc>>()
                .unwrap_or_else(|_| Utc::now()),
        }
    }
}

// ── User queries ──────────────────────────────────────────────────────────────

pub async fn insert_user(client: &Client, user: &User) -> Result<(), AppError> {
    let is_active: u8 = if user.is_active { 1 } else { 0 };
    client
        .query(
            "INSERT INTO exchange.users
             (id, username, password_hash, role, balance_usdc, created_at, is_active)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&user.id)
        .bind(&user.username)
        .bind(&user.password_hash)
        .bind(user.role.as_str())
        .bind(user.balance_usdc.to_string())
        .bind(user.created_at.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
        .bind(is_active)
        .execute()
        .await
        .map_err(AppError::Clickhouse)
}

pub async fn get_user_by_username(
    client: &Client,
    username: &str,
) -> Result<Option<User>, AppError> {
    let rows: Vec<UserRow> = client
        .query(
            "SELECT id, username, password_hash, role, balance_usdc,
                    toString(created_at) as created_at, is_active
             FROM exchange.users FINAL
             WHERE username = ?
             LIMIT 1",
        )
        .bind(username)
        .fetch_all()
        .await
        .map_err(AppError::Clickhouse)?;

    Ok(rows.into_iter().next().map(UserRow::into_model))
}

pub async fn get_user_by_id(client: &Client, id: &str) -> Result<Option<User>, AppError> {
    let rows: Vec<UserRow> = client
        .query(
            "SELECT id, username, password_hash, role, balance_usdc,
                    toString(created_at) as created_at, is_active
             FROM exchange.users FINAL
             WHERE id = ?
             LIMIT 1",
        )
        .bind(id)
        .fetch_all()
        .await
        .map_err(AppError::Clickhouse)?;

    Ok(rows.into_iter().next().map(UserRow::into_model))
}

pub async fn list_users(client: &Client) -> Result<Vec<User>, AppError> {
    let rows: Vec<UserRow> = client
        .query(
            "SELECT id, username, password_hash, role, balance_usdc,
                    toString(created_at) as created_at, is_active
             FROM exchange.users FINAL
             WHERE is_active = 1
             ORDER BY created_at DESC",
        )
        .fetch_all()
        .await
        .map_err(AppError::Clickhouse)?;

    Ok(rows.into_iter().map(UserRow::into_model).collect())
}

pub async fn update_user_balance(client: &Client, user: &User) -> Result<(), AppError> {
    insert_user(client, user).await
}

// ── Order queries ─────────────────────────────────────────────────────────────

pub async fn insert_order(client: &Client, order: &Order) -> Result<(), AppError> {
    client
        .query(
            "INSERT INTO exchange.orders
             (id, user_id, symbol, side, order_type, price, amount, total_usdc,
              limit_price, status, reject_reason, realized_pnl, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&order.id)
        .bind(&order.user_id)
        .bind(&order.symbol)
        .bind(order.side.as_str())
        .bind(order.order_type.as_str())
        .bind(order.price.to_string())
        .bind(order.amount.to_string())
        .bind(order.total_usdc.to_string())
        .bind(order.limit_price.map(|p| p.to_string()).unwrap_or_default())
        .bind(order.status.as_str())
        .bind(order.reject_reason.as_deref().unwrap_or(""))
        .bind("") // realized_pnl: not computed here
        .bind(order.created_at.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
        .bind(order.updated_at.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
        .execute()
        .await
        .map_err(AppError::Clickhouse)
}

pub async fn get_order_by_id(client: &Client, order_id: &str) -> Result<Option<Order>, AppError> {
    let rows: Vec<OrderRow> = client
        .query(
            "SELECT id, user_id, symbol, side, order_type, price, amount, total_usdc,
                    limit_price, status, reject_reason, realized_pnl,
                    toString(created_at) as created_at, toString(updated_at) as updated_at
             FROM exchange.orders FINAL
             WHERE id = ?
             LIMIT 1",
        )
        .bind(order_id)
        .fetch_all()
        .await
        .map_err(AppError::Clickhouse)?;

    Ok(rows.into_iter().next().map(OrderRow::into_model))
}

/// Re-inserts the order with a new status. ReplacingMergeTree(updated_at) keeps the
/// latest version at query time (FINAL).
pub async fn update_order_status(
    client: &Client,
    order: &Order,
    new_status: OrderStatus,
) -> Result<(), AppError> {
    let mut updated = order.clone();
    updated.status = new_status;
    updated.updated_at = Utc::now();
    insert_order(client, &updated).await
}

/// Re-inserts the order as Filled with the execution price and total.
pub async fn update_order_fill(
    client: &Client,
    order: &Order,
    exec_price: Decimal,
    total_usdc: Decimal,
) -> Result<(), AppError> {
    let mut filled = order.clone();
    filled.status = OrderStatus::Filled;
    filled.price = exec_price;
    filled.total_usdc = total_usdc;
    filled.updated_at = Utc::now();
    insert_order(client, &filled).await
}

pub async fn list_orders_for_user(
    client: &Client,
    user_id: &str,
    limit: u32,
) -> Result<Vec<Order>, AppError> {
    let rows: Vec<OrderRow> = client
        .query(
            "SELECT id, user_id, symbol, side, order_type, price, amount, total_usdc,
                    limit_price, status, reject_reason, realized_pnl,
                    toString(created_at) as created_at, toString(updated_at) as updated_at
             FROM exchange.orders FINAL
             WHERE user_id = ?
             ORDER BY created_at DESC
             LIMIT ?",
        )
        .bind(user_id)
        .bind(limit)
        .fetch_all()
        .await
        .map_err(AppError::Clickhouse)?;

    Ok(rows.into_iter().map(OrderRow::into_model).collect())
}

pub async fn list_all_orders(client: &Client, limit: u32) -> Result<Vec<Order>, AppError> {
    let rows: Vec<OrderRow> = client
        .query(
            "SELECT id, user_id, symbol, side, order_type, price, amount, total_usdc,
                    limit_price, status, reject_reason, realized_pnl,
                    toString(created_at) as created_at, toString(updated_at) as updated_at
             FROM exchange.orders FINAL
             ORDER BY created_at DESC
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all()
        .await
        .map_err(AppError::Clickhouse)?;

    Ok(rows.into_iter().map(OrderRow::into_model).collect())
}

// ── Position queries ──────────────────────────────────────────────────────────

pub async fn upsert_position(client: &Client, pos: &Position) -> Result<(), AppError> {
    client
        .query(
            "INSERT INTO exchange.positions
             (user_id, symbol, quantity, avg_buy_price, updated_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&pos.user_id)
        .bind(&pos.symbol)
        .bind(pos.quantity.to_string())
        .bind(pos.avg_buy_price.to_string())
        .bind(pos.updated_at.format("%Y-%m-%d %H:%M:%S%.6f").to_string())
        .execute()
        .await
        .map_err(AppError::Clickhouse)
}

pub async fn get_positions_for_user(
    client: &Client,
    user_id: &str,
) -> Result<Vec<Position>, AppError> {
    let rows: Vec<PositionRow> = client
        .query(
            "SELECT user_id, symbol, quantity, avg_buy_price,
                    toString(updated_at) as updated_at
             FROM exchange.positions FINAL
             WHERE user_id = ?
               AND toFloat64OrZero(quantity) > 0",
        )
        .bind(user_id)
        .fetch_all()
        .await
        .map_err(AppError::Clickhouse)?;

    Ok(rows.into_iter().map(PositionRow::into_model).collect())
}
