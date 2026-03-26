use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ── UserRole ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    Admin,
    Trader,
}

impl UserRole {
    pub fn as_str(self) -> &'static str {
        match self {
            UserRole::Admin => "admin",
            UserRole::Trader => "trader",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "admin" => Some(UserRole::Admin),
            "trader" => Some(UserRole::Trader),
            _ => None,
        }
    }
}

impl std::fmt::Display for UserRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── OrderType ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
#[serde(rename_all = "lowercase")]
pub enum OrderType {
    #[default]
    Market,
    Limit,
    #[serde(rename = "stop_limit")]
    StopLimit,
}

impl OrderType {
    pub fn as_str(self) -> &'static str {
        match self {
            OrderType::Market => "market",
            OrderType::Limit => "limit",
            OrderType::StopLimit => "stop_limit",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "market" => Some(OrderType::Market),
            "limit" => Some(OrderType::Limit),
            "stop_limit" => Some(OrderType::StopLimit),
            _ => None,
        }
    }
}

impl std::fmt::Display for OrderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Side ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_str(self) -> &'static str {
        match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "buy" => Some(Side::Buy),
            "sell" => Some(Side::Sell),
            _ => None,
        }
    }
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── OrderStatus ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum OrderStatus {
    Filled,
    Rejected,
    Pending,  // open limit order waiting to be matched
    Canceled, // limit order canceled by the user
}

impl OrderStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OrderStatus::Filled => "filled",
            OrderStatus::Rejected => "rejected",
            OrderStatus::Pending => "pending",
            OrderStatus::Canceled => "canceled",
        }
    }
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "filled" => Some(OrderStatus::Filled),
            "rejected" => Some(OrderStatus::Rejected),
            "pending" => Some(OrderStatus::Pending),
            "canceled" => Some(OrderStatus::Canceled),
            _ => None,
        }
    }
}

impl std::fmt::Display for OrderStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── User ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub role: UserRole,
    pub balance_usdc: Decimal,
    pub created_at: DateTime<Utc>,
    pub is_active: bool,
}

// ── Order ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub id: String,
    pub user_id: String,
    pub symbol: String,
    pub side: Side,
    pub order_type: OrderType,
    /// The requested limit price. None for market orders.
    pub limit_price: Option<Decimal>,
    /// Execution price. Zero until filled for limit orders.
    pub price: Decimal,
    pub amount: Decimal,
    /// price × amount. Zero until filled for limit orders.
    pub total_usdc: Decimal,
    pub status: OrderStatus,
    pub reject_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Version column for ReplacingMergeTree; bumped on every status change.
    pub updated_at: DateTime<Utc>,
}

// ── Position ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub user_id: String,
    pub symbol: String,
    pub quantity: Decimal,
    pub avg_buy_price: Decimal,
    pub updated_at: DateTime<Utc>,
}

// ── Requests ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct PlaceOrderRequest {
    pub symbol: String,
    pub side: Side,
    pub amount: Decimal,
    /// Defaults to Market when omitted from JSON.
    #[serde(default)]
    pub order_type: OrderType,
    /// Required when order_type is Limit.
    pub limit_price: Option<Decimal>,
}

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub password: String,
    pub role: String,
    pub initial_balance: Option<Decimal>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateBalanceRequest {
    pub balance_usdc: Decimal,
}

// ── Responses ─────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub user_id: String,
    pub username: String,
    pub role: UserRole,
}

#[derive(Debug, Serialize)]
pub struct AccountResponse {
    pub user_id: String,
    pub username: String,
    pub role: UserRole,
    pub balance_usdc: Decimal,
    pub positions: Vec<Position>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UnrealizedPositionPnl {
    pub pnl: Decimal,
    pub quantity: Decimal,
    pub avg_buy_price: Decimal,
    pub current_price: Decimal,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PnlByOrderType {
    pub market: Decimal,
    pub limit: Decimal,
    pub stop_limit: Decimal,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RealizedPnlDetail {
    pub symbol: String,
    pub order_type: OrderType,
    pub pnl: Decimal,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PnlStats {
    pub realized_total: Decimal,
    pub realized_by_symbol: std::collections::HashMap<String, Decimal>,
    pub realized_by_order_type: PnlByOrderType,
    pub realized_details: Vec<RealizedPnlDetail>,
    pub unrealized_total: Decimal,
    pub unrealized_by_symbol: std::collections::HashMap<String, UnrealizedPositionPnl>,
}

// ── Live price snapshot (from WS client) ─────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct MarketDataMessage {
    pub price: f64,
    pub symbol: String,
}

/// Returns a new UUID string.
pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}
