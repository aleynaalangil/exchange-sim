# exchange-sim — Architecture & Design Documentation

## What is this service?

`exchange-sim` is a simulated crypto exchange backend. It accepts authenticated trade orders from users, executes them at live market prices sourced from the `fpga-hft-data-generator` service, and persists all account state (users, balances, orders, positions) in ClickHouse. It is intentionally a single-process, stateless HTTP service — no message broker, no cache server.

---

## Module Map

```
src/
├── main.rs        — Server bootstrap, route registration, AppState
├── config.rs      — Environment variable loading (including SYMBOLS)
├── models.rs      — Domain types: User, Order, Position, enums, request/response shapes
├── error.rs       — Unified AppError enum → HTTP response mapping
├── auth.rs        — JWT creation, verification, jti-based revocation
├── db.rs          — All ClickHouse queries (users, orders, positions)
├── engine.rs      — Order execution logic (market + limit, position math)
├── order_book.rs  — In-memory limit order book + background matcher task
├── ws_client.rs   — WebSocket consumer that keeps the live price cache warm
└── api/
    ├── mod.rs
    ├── auth.rs    — POST /auth/login, POST /auth/register, POST /auth/logout
    ├── account.rs — GET /account
    ├── orders.rs  — POST /orders, GET /orders, DELETE /orders/{id}
    └── admin.rs   — Admin-only user/balance/order management
```

---

## Architecture Decisions

### 1. In-process price cache (DashMap) instead of Redis

The live price cache is a `DashMap<String, f64>` held in memory within the exchange-sim process, fed by a background Tokio task that consumes the HFT generator's WebSocket feed.

**Why not Redis?**
- There is exactly one instance of exchange-sim. A shared external cache adds a network hop for every order with zero benefit in a single-node deployment.
- `DashMap` is a lock-free concurrent hashmap. Price reads during order execution are contention-free and sub-microsecond.
- The data is ephemeral by nature — if the process restarts it reconnects to the WebSocket and the cache re-warms within milliseconds. Persistence is not useful here.
- Redis would only be justified when scaling exchange-sim horizontally (multiple instances behind a load balancer), because then each instance would need to read from a shared source of truth rather than maintaining its own feed connection.

### 2. JWT auth with in-memory revocation (jti blocklist)

Tokens encode `user_id`, `role`, and a unique `jti` (JWT ID). Every request is self-contained — the handler verifies the token's HMAC signature and expiry, then checks the jti against an in-memory `DashMap<jti, expiry>`.

`POST /auth/logout` inserts the token's jti into the blocklist. A background task purges expired entries every 5 minutes to bound memory growth.

**Tradeoff vs Redis:** The blocklist is in-process only. If the service restarts, revoked tokens become valid again until their natural expiry. For production, this blocklist should be in Redis so it survives restarts and is shared across multiple instances.

### 3. ClickHouse for all persistent state

ClickHouse is primarily an OLAP / time-series database, not a transactional OLTP store. It was chosen because the sibling service (`fpga-hft-data-generator`) already uses it for tick and OHLCV storage, and keeping a single infrastructure dependency was a deliberate simplicity trade-off.

**Consequences of this choice:**

| Pattern | How it is handled in code |
|---|---|
| "Upsert" a user balance | Re-insert a new row with a later `created_at` + `ReplacingMergeTree` deduplicates by `id` on merge. All queries use `FINAL` to force dedup at read time. |
| "Upsert" a position | Same pattern — `ReplacingMergeTree` on `(user_id, symbol)`, queries use `FINAL`. |
| "Update" an order status | Orders table uses `ReplacingMergeTree(updated_at)` with `ORDER BY (user_id, id)`. Re-inserting with a newer `updated_at` causes the new row to win at query time (FINAL). |
| Uniqueness constraint on username | No DB-level unique index — enforced by a `tokio::Mutex` in `AppState` that serialises the check-then-insert. |

### 4. Per-user mutex for order serialisation (atomicity improvement)

`AppState` holds `user_locks: DashMap<user_id, Arc<Mutex<()>>>`. The `place_order` and `cancel_order` handlers acquire the per-user lock before reading the user from DB, hold it through both ClickHouse writes, and release it after.

This prevents concurrent orders from the same user both passing the balance check. It does not make the two ClickHouse writes fully atomic — a crash between `insert_order` and `update_user_balance` would leave the order recorded but the balance not decremented. The correct fix for this requires either Postgres (with real transactions) or event-sourcing (deriving balance from order history). The per-user mutex is the pragmatic improvement available within the ClickHouse constraint.

### 5. Symbols loaded from config (SYMBOLS env var)

Trading pairs are no longer hard-coded. `Config.symbols` is populated from the `SYMBOLS` environment variable (comma-separated, e.g. `SOL/USDC,BTC/USDC`). The engine accepts both slash and dash forms (`SOL-USDC` or `SOL/USDC`). Adding a new symbol requires only an env var change and restart — no code change.

### 6. In-memory limit order book with background matcher

`OrderBook` is an `Arc<RwLock<Vec<PendingLimitOrder>>>` shared between request handlers and a background Tokio task (`order_book::run_matcher`).

**Placement flow:**
- Funds are locked immediately: USDC for buy limits (`limit_price × amount`), position quantity for sell limits.
- A `PendingLimitOrder` is added to the book; the order is persisted as `Pending` in ClickHouse.

**Matching flow (every 500ms):**
- The matcher calls `drain_fillable`, which atomically removes all fillable orders from the book in a single write-lock pass.
- For each fill: position (buys) or USDC (sells) is credited; excess locked USDC is refunded; the order row is re-inserted with `status = Filled` (ReplacingMergeTree keeps the latest).

**Cancellation flow (`DELETE /orders/{id}`):**
- The handler acquires the per-user lock to prevent a race with the background matcher.
- `order_book.remove` removes the order from the book and returns the locked funds metadata.
- Locked funds are returned before the order is marked `Canceled` in ClickHouse.

**Known gap:** `drain_fillable` removes from the book before acquiring the per-user lock inside the matcher. There is a narrow window where a cancel handler could find the order absent from the book (taken by the matcher) but the DB still showing `Pending` (matcher hasn't written yet). In this case, cancel returns a clear error: "Order cannot be canceled: status is pending". No state corruption occurs.

### 7. Username uniqueness global mutex

`user_creation_lock: Arc<tokio::sync::Mutex<()>>` in `AppState` serialises both `/auth/register` and `/admin/users` (POST). The bcrypt hash is computed before acquiring the lock (it is expensive and doesn't need to be inside the critical section). The lock covers the uniqueness check + insert only.

### 8. Engine as a plain struct, not a service trait

`Engine` holds `client`, `prices`, and `symbols`. There is no trait abstraction — there is only one implementation, and a trait would add indirection without enabling anything.

### 9. AppError as a thiserror enum mapped to HTTP responses

All error paths return `AppError`. The `ResponseError` implementation maps each variant to the correct HTTP status code and a consistent JSON body `{ "error": "..." }`. Handlers never call `HttpResponse::BadRequest()` directly.

### 10. Exponential back-off on ClickHouse startup

The service polls ClickHouse with `SELECT 1` on startup, backing off exponentially (2^attempt seconds, capped at 16s, up to 10 attempts). This makes `docker-compose up` reliable without `depends_on: condition: service_healthy`.

---

## Request Lifecycle — Place Market Order

```
POST /api/v1/orders  { symbol, side, amount, order_type: "market" }
  │
  ├─ extract_claims()           — verify JWT signature + expiry + jti not revoked
  ├─ get_user_lock(user_id)     — acquire per-user Mutex
  ├─ db::get_user_by_id()       — load current balance (FINAL)
  ├─ engine.place_order()
  │   ├─ normalize_symbol()     — accept "SOL-USDC" or "SOL/USDC"
  │   ├─ prices.get(&symbol)    — read live price from DashMap (lock-free)
  │   ├─ balance / position check → reject if insufficient
  │   ├─ mutate user.balance_usdc in memory
  │   ├─ pub_update_position_buy/sell()
  │   │   ├─ db::get_positions_for_user()
  │   │   └─ db::upsert_position()
  │   ├─ db::insert_order()     — persist order record (status: filled)
  │   └─ db::update_user_balance()
  ├─ release per-user Mutex
  └─ return 201 Order JSON
```

## Request Lifecycle — Place Limit Order

```
POST /api/v1/orders  { symbol, side, amount, order_type: "limit", limit_price }
  │
  ├─ extract_claims() + per-user lock
  ├─ db::get_user_by_id()
  ├─ engine.execute_limit()
  │   ├─ Buy: deduct limit_price × amount from balance (lock USDC)
  │   │  Sell: deduct position quantity (lock asset)
  │   ├─ db::insert_order()     — status: pending, price: 0
  │   └─ db::update_user_balance()
  ├─ order_book.add(PendingLimitOrder)
  ├─ release per-user Mutex
  └─ return 200 Order JSON (status: pending)

  [background — every 500ms]
  order_book::run_matcher()
    ├─ drain_fillable(&prices)  — atomically remove eligible orders
    └─ for each fillable order:
        ├─ acquire per-user lock
        ├─ db::update_order_fill()  — re-insert with status: filled, exec_price
        ├─ credit position (buy) or USDC (sell)
        ├─ refund excess locked USDC (buy only)
        └─ release per-user lock
```

---

## API Surface

### Public (no auth)
| Method | Path | Description |
|---|---|---|
| GET | `/api/v1/health` | Liveness check, returns live symbol count |
| GET | `/api/v1/symbols` | List supported trading pairs (from SYMBOLS env var) |
| GET | `/api/v1/prices` | Current price snapshot from HFT cache |

### Authenticated (Bearer JWT)
| Method | Path | Description |
|---|---|---|
| POST | `/api/v1/auth/register` | Create trader account (10,000 USDC starting balance) |
| POST | `/api/v1/auth/login` | Bcrypt password check → JWT with jti |
| POST | `/api/v1/auth/logout` | Revoke token (add jti to blocklist) |
| GET | `/api/v1/account` | Balance + open positions |
| POST | `/api/v1/orders` | Place market or limit order |
| GET | `/api/v1/orders` | Order history (default limit 50, max 500) |
| DELETE | `/api/v1/orders/{id}` | Cancel a pending limit order, return locked funds |

### Admin only (Bearer JWT, role = admin)
| Method | Path | Description |
|---|---|---|
| GET | `/api/v1/admin/users` | List all active users |
| POST | `/api/v1/admin/users` | Create user with any role |
| PATCH | `/api/v1/admin/users/{id}/balance` | Set a user's USDC balance |
| GET | `/api/v1/admin/orders` | All orders (default limit 100, max 1000) |
| GET | `/api/v1/admin/symbols` | Supported symbols |
| GET | `/api/v1/admin/prices` | Live price snapshot |

---

## Remaining Limitations

### Atomicity across two ClickHouse writes
The per-user mutex prevents concurrent-request races, but it does not protect against a crash between `insert_order` and `update_user_balance`. The correct fix requires either a transactional database (Postgres) or event-sourcing (balance derived from order history). Accepted limitation for this simulator.

### Token blocklist does not survive restarts
The revocation list is in-process memory. A service restart clears it, allowing revoked tokens to be used again until their natural expiry. Production fix: back the blocklist with Redis.

### No rate limiting
A user can submit unlimited orders per second. Production fix: per-user Redis counter with a sliding window TTL.

### No TLS
Plain HTTP. Production: TLS termination at nginx/envoy or Rustls directly.

### CORS is fully open
`allow_any_origin()` is set for development. Production: restrict to the known frontend origin.

### Limit order matching is price-only, no time priority
The matcher fills limit orders based purely on whether the market price crossed the limit price. When multiple orders are eligible, they are all filled in the same 500ms tick with no FIFO ordering.

### ClickHouse schema change requires container recreation
The `exchange.orders` table was changed from `MergeTree` to `ReplacingMergeTree(updated_at)`. Existing deployments must run `docker-compose down -v && docker-compose up -d` to apply this migration.

---

## Dependencies

| Crate | Purpose |
|---|---|
| `actix-web` | HTTP server |
| `actix-cors` | CORS middleware |
| `clickhouse` | ClickHouse HTTP client |
| `dashmap` | Lock-free concurrent hashmap (price cache, user locks, token blocklist) |
| `tokio-tungstenite` | WebSocket client (HFT feed consumer) |
| `jsonwebtoken` | JWT sign/verify (HS256) |
| `bcrypt` | Password hashing |
| `rust_decimal` | Exact decimal arithmetic for balances/prices |
| `uuid` | Order, user, and jti ID generation |
| `thiserror` | Error enum boilerplate |
| `tracing` + `tracing-subscriber` | Structured logging |
| `dotenvy` | `.env` file loading |
| `tokio-util` | `CancellationToken` for graceful shutdown |
