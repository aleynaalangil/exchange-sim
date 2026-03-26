mod api;
mod auth;
mod config;
mod db;
mod engine;
mod error;
mod models;
mod order_book;
mod ws_client;

use actix_cors::Cors;
use actix_web::{get, middleware, web, App, HttpResponse, HttpServer};
use auth::TokenBlocklist;
use chrono::Utc;
use clickhouse::Client;
use config::Config;
use dashmap::DashMap;
use order_book::{new_user_locks, OrderBook, UserLocks};
use std::sync::Arc;
use tokio::sync::Mutex as TokioMutex;
use tokio::time::{sleep, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use ws_client::PriceCache;

// ── Shared application state ──────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub db: Client,
    pub prices: PriceCache,
    pub config: Arc<Config>,
    /// Per-user mutex — serialises concurrent order mutations for the same user.
    pub user_locks: UserLocks,
    /// Global mutex — prevents TOCTOU races on username uniqueness checks.
    pub user_creation_lock: Arc<TokioMutex<()>>,
    /// In-memory JWT revocation list: jti → expiry (Unix seconds).
    pub token_blocklist: Arc<TokenBlocklist>,
    /// In-memory pending limit orders.
    pub order_book: OrderBook,
}

// ── Public endpoints ──────────────────────────────────────────────────────────

#[get("/api/v1/health")]
async fn health(state: web::Data<AppState>) -> HttpResponse {
    let price_count = state.prices.len();
    HttpResponse::Ok().json(serde_json::json!({
        "status": "ok",
        "service": "exchange-sim",
        "live_symbols": price_count,
    }))
}

#[get("/api/v1/symbols")]
async fn symbols(state: web::Data<AppState>) -> HttpResponse {
    HttpResponse::Ok().json(serde_json::json!({
        "symbols": state.config.symbols,
    }))
}

#[get("/api/v1/prices")]
async fn get_prices(state: web::Data<AppState>) -> HttpResponse {
    let snapshot: serde_json::Map<String, serde_json::Value> = state
        .prices
        .iter()
        .map(|entry| (entry.key().clone(), serde_json::json!(*entry.value())))
        .collect();
    HttpResponse::Ok().json(snapshot)
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("exchange_sim=info")),
        )
        .init();

    let config = Arc::new(Config::from_env());

    let db_client = db::make_client(
        &config.clickhouse_url,
        &config.clickhouse_user,
        &config.clickhouse_password,
        &config.clickhouse_db,
    );

    // Wait until ClickHouse is ready
    let mut attempt = 0u32;
    loop {
        match db::check_ready(&db_client).await {
            Ok(_) => break,
            Err(e) => {
                attempt += 1;
                if attempt >= 10 {
                    error!("ClickHouse unavailable after 10 attempts: {}", e);
                    error!("Make sure Docker is running: docker-compose up -d");
                    std::process::exit(1);
                }
                let delay = Duration::from_secs(2u64.pow(attempt.min(4)));
                warn!(
                    "ClickHouse not ready (attempt {}/10): {}. Retrying in {:?}…",
                    attempt, e, delay
                );
                sleep(delay).await;
            }
        }
    }

    let price_cache = ws_client::new_price_cache();
    let cancel = CancellationToken::new();

    // Spawn HFT WebSocket price feed
    {
        let prices = price_cache.clone();
        let url = config.hft_ws_url.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            ws_client::run_ws_client(url, prices, cancel).await;
        });
    }

    let order_book = OrderBook::new();
    let user_locks = new_user_locks();
    let token_blocklist: Arc<TokenBlocklist> = Arc::new(DashMap::new());

    let state = web::Data::new(AppState {
        db: db_client,
        prices: price_cache,
        config: config.clone(),
        user_locks: user_locks.clone(),
        user_creation_lock: Arc::new(TokioMutex::new(())),
        token_blocklist: token_blocklist.clone(),
        order_book: order_book.clone(),
    });

    // Spawn limit order matcher
    {
        let book = order_book.clone();
        let prices = state.prices.clone();
        let db = state.db.clone();
        let locks = user_locks.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            book.run_matcher(prices, db, locks, cancel).await;
        });
    }

    // Spawn token blocklist cleanup (every 5 minutes, removes expired entries)
    {
        let blocklist = token_blocklist.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = sleep(Duration::from_secs(300)) => {
                        let now = Utc::now().timestamp() as u64;
                        blocklist.retain(|_, exp| *exp > now);
                    }
                }
            }
        });
    }

    let bind_addr = format!("{}:{}", config.host, config.port);
    info!("exchange-sim listening on http://{}", bind_addr);

    HttpServer::new(move || {
        let cors = Cors::default()
            .allow_any_origin()
            .allow_any_method()
            .allow_any_header()
            .max_age(3600);

        App::new()
            .wrap(cors)
            .wrap(middleware::Logger::default())
            .app_data(state.clone())
            // Public
            .service(health)
            .service(symbols)
            .service(get_prices)
            // Auth + protected routes
            .service(
                web::scope("/api/v1")
                    .service(api::auth::login)
                    .service(api::auth::register)
                    .service(api::auth::logout)
                    .service(api::account::get_account)
                    .service(api::account::get_pnl)
                    .service(api::orders::place_order)
                    .service(api::orders::list_orders)
                    .service(api::orders::cancel_order)
                    // Admin
                    .service(api::admin::list_users)
                    .service(api::admin::create_user)
                    .service(api::admin::update_balance)
                    .service(api::admin::list_all_orders)
                    .service(api::admin::list_symbols)
                    .service(api::admin::live_prices),
            )
    })
    .bind(&bind_addr)?
    .run()
    .await?;

    cancel.cancel();
    Ok(())
}
