use std::env;

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub clickhouse_url: String,
    pub clickhouse_user: String,
    pub clickhouse_password: String,
    pub clickhouse_db: String,
    pub hft_ws_url: String,
    pub jwt_secret: String,
    pub jwt_expiry_hours: u64,
    /// Trading pairs loaded from the SYMBOLS env var (comma-separated).
    /// Defaults to "SOL/USDC,BTC/USDC".
    pub symbols: Vec<String>,
}

impl Config {
    pub fn from_env() -> Self {
        let symbols = env::var("SYMBOLS")
            .unwrap_or_else(|_| "SOL/USDC,BTC/USDC".to_string())
            .split(',')
            .map(|s| s.trim().to_uppercase().replace('-', "/"))
            .filter(|s| !s.is_empty())
            .collect();

        Self {
            host: env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string()),
            port: env::var("PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8081),
            clickhouse_url: env::var("CLICKHOUSE_URL")
                .unwrap_or_else(|_| "http://localhost:8123".to_string()),
            clickhouse_user: env::var("CLICKHOUSE_USER")
                .unwrap_or_else(|_| "exchange_user".to_string()),
            clickhouse_password: env::var("CLICKHOUSE_PASSWORD")
                .unwrap_or_else(|_| "exchange_pass".to_string()),
            clickhouse_db: env::var("CLICKHOUSE_DB").unwrap_or_else(|_| "exchange".to_string()),
            hft_ws_url: env::var("HFT_WS_URL")
                .unwrap_or_else(|_| "ws://localhost:8080/v1/feed".to_string()),
            jwt_secret: env::var("JWT_SECRET")
                .unwrap_or_else(|_| "change-me-in-production-secret-key".to_string()),
            jwt_expiry_hours: env::var("JWT_EXPIRY_HOURS")
                .unwrap_or_else(|_| "24".to_string())
                .parse()
                .unwrap_or(24),
            symbols,
        }
    }
}
