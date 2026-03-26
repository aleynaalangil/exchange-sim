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

## Real-Time Price Feed & Order Execution Consistency

### How the price feed works

`ws_client.rs` runs a background `tokio` task that maintains a persistent WebSocket connection to `fpga-hft-data-generator /v1/feed`. Each `MarketDataMessage` received updates the in-process `DashMap<String, f64>` price cache atomically:

```
fpga-hft-data-generator (every 10ms)
  └─ broadcast tick → WebSocket frame
       └─ ws_client.rs (tokio task)
            └─ parse price from MarketDataMessage
                 └─ prices.insert(symbol, price)  ← DashMap, lock-free write

POST /api/v1/orders handler (any thread)
  └─ prices.get(&symbol)  ← DashMap, lock-free read, sub-microsecond
       └─ use exec_price for balance check and order record
```

The DashMap decouples the write frequency (100/sec from the WS feed) from the read frequency (one read per order). Order handlers never wait for a price update and are never blocked by a price write.

### Consistency guarantees for order execution

**Price freshness.** The price used for execution is at most 10ms stale (one tick interval). For a simulated exchange this is acceptable — real exchanges use last-trade price which can also be stale by network propagation time.

**Per-user atomicity.** Each order handler acquires the user's `Mutex` before reading the balance and holds it through both ClickHouse writes (order record + balance update). This prevents two concurrent orders from the same user both passing the balance check against the same pre-order balance. The Mutex makes the read-check-write sequence atomic at the application level.

**What is not atomic.** The two ClickHouse writes (`insert_order` and `update_user_balance`) are separate HTTP requests to ClickHouse. There is no distributed transaction. A process crash between the two writes would leave the order recorded but the balance not decremented. The per-user Mutex prevents concurrent races but not crash-induced inconsistency. This is an acknowledged limitation documented in `Remaining Limitations`.

**Reconnection behaviour.** If the WebSocket connection to `fpga-hft-data-generator` drops, the price cache retains the last known value for each symbol. Incoming orders during a reconnect window will execute against a stale price. The reconnection logic in `ws_client.rs` attempts to re-establish the connection immediately. For a simulator, serving orders at a slightly stale price is preferable to refusing all orders during a brief network interruption.

**State read at order time vs display time.** The frontend reads prices from its own Zustand store (populated by its own WebSocket connection). The exchange-sim reads prices from its own DashMap (populated by its own WebSocket connection). Both connect to the same `fpga-hft-data-generator` feed, so they see the same tick sequence. The price displayed to the user and the price used for execution are derived from the same source and differ only by the propagation delay between the two WebSocket connections — typically under 1ms on a local network.

---

## Technology Choices

### Rust

**Performance:** Order execution is on the critical latency path — a user's balance check, position update, and order record must all complete before the HTTP response returns. Rust executes this without GC pauses. At market-order rates (tens per second per user) GC pauses in Go or the JVM would introduce occasional multi-millisecond spikes visible as slow trade confirmations.

**Scalability:** `async/await` on tokio handles many concurrent HTTP requests and the background matcher task on a fixed thread pool. The per-user `Mutex` pattern (locking only the relevant user's state, not a global lock) scales linearly with user count — 1000 users means 1000 independent locks, not one bottleneck.

**Maintainability:** The type system enforces the domain model at compile time. `OrderStatus`, `Side`, `OrderType` are enums — a handler cannot accidentally pass `"fille"` where `"filled"` is expected. `thiserror` derives clean error types with no boilerplate. The borrow checker prevents use-after-free in the order book's concurrent drain-and-fill pattern.

**Alternatives considered:** Go — simpler concurrency, but GC pauses and weaker type safety for the domain model. Node.js + TypeScript — fast for I/O-bound work but single-threaded CPU execution is a ceiling risk; also shares a runtime with the frontend, making it harder to reason about independently. Python (FastAPI) — insufficient throughput for order execution at scale.

---

### actix-web

**Performance:** Same reasoning as `fpga-hft-data-generator` — top TechEmpower ranking, multi-threaded tokio workers. For the exchange-sim the additional concern is that order placement involves two ClickHouse writes and a position read. actix-web's async handlers allow all non-conflicting user requests to run concurrently while the per-user mutex serialises only the writes for the same user.

**Scalability:** `web::Data<Arc<T>>` injects shared state (ClickHouse client, price DashMap, order book, user locks) into handlers without cloning the data. Adding new endpoints (e.g. a new admin route) requires only a new handler function and one `.service()` call in `main.rs`.

**Maintainability:** The `AppError` enum with `ResponseError` implementation means all error paths produce consistent `{ "error": "..." }` JSON responses. Handlers are pure functions that receive typed extractors — no request parsing boilerplate inside business logic.

**Alternatives considered:** Axum — slightly more ergonomic type-state extractors, but actix-web's middleware ecosystem (CORS, rate limiting) was more complete at project start. Both are valid choices at this scale.

---

### ClickHouse (as the sole persistent store)

**Performance:** ClickHouse INSERT throughput (millions of rows/sec) far exceeds the exchange-sim's write rate (tens of orders/sec). The cost paid is on reads — `FINAL` on `ReplacingMergeTree` tables is slower than a primary-key lookup in PostgreSQL. This is acceptable because account reads (balance, positions, order history) are user-initiated and not on the sub-millisecond path.

**Scalability:** Using one database for all services (market data in `hft_dashboard`, account state in `exchange`) eliminates a cross-service join problem. The `exchange_user` ClickHouse role has `SELECT` on `hft_dashboard.*` — if the exchange-sim ever needs to query OHLCV data directly, no new connection or API call is required.

**Maintainability:** Single infrastructure dependency. Running `docker-compose up` brings up ClickHouse and both services — no PostgreSQL instance, no Redis, no message broker to operate. `clickhouse-config/init.sql` is the single source of truth for all table schemas.

**Alternatives considered:** PostgreSQL — ACID transactions would eliminate the "crash between two writes" limitation and make the per-user mutex unnecessary. Chosen against because ClickHouse was already required for market data, and adding a second DB engine increases operational complexity. SQLite — no concurrent write support. MongoDB — schemaless is a liability for financial data where every field must be accounted for.

---

### jsonwebtoken (JWT / HS256)

**Performance:** HMAC-SHA256 signature verification is a single CPU operation — microseconds per request. There is no database roundtrip for auth on the hot path. Only the jti blocklist check (a DashMap lookup) adds any overhead, and DashMap reads are lock-free.

**Scalability:** Stateless tokens scale horizontally — any instance of exchange-sim can verify a token without contacting a central auth service. The jti blocklist is the only stateful component, and it is intentionally in-process (acknowledged limitation for single-instance deployments).

**Maintainability:** The `Claims` struct defines the token payload as a typed Rust struct. Adding a new claim field is a one-line change to the struct and the `encode` call. Token expiry is enforced by the library — no manual timestamp comparison in handlers.

**Alternatives considered:** OAuth2 / OpenID Connect — appropriate for multi-tenant or federated auth, significant added complexity for a simulator. Session cookies — require sticky sessions or a shared session store, adds complexity. API keys — simpler but no expiry mechanism without a DB lookup on every request.

---

### bcrypt (password hashing)

**Performance:** bcrypt is intentionally slow (work factor controls cost). The hash computation happens only at login and registration — never on the order execution path. The per-request cost is zero (JWT handles subsequent auth).

**Scalability:** Work factor can be increased as hardware improves without changing stored hashes — bcrypt automatically encodes the factor in the hash string. The `user_creation_lock` mutex ensures bcrypt is computed outside the critical section (it is expensive and does not need DB access), so concurrent registrations do not block each other during the hash.

**Maintainability:** The bcrypt crate's `verify(password, hash)` API has no footguns — it extracts the salt from the stored hash automatically. Constant-time comparison is built in, preventing timing attacks.

**Alternatives considered:** Argon2id — more modern, memory-hard, recommended by OWASP over bcrypt for new projects. Either is acceptable for a simulator. scrypt — similar to Argon2id. MD5/SHA — never acceptable for password storage.

---

### tokio-tungstenite (WebSocket client)

**Performance:** The price cache warm-up WebSocket runs as a background tokio task — no dedicated thread, no blocking. Price updates arrive as async messages and update the `DashMap` atomically without pausing any request handler.

**Scalability:** One persistent WebSocket connection to `fpga-hft-data-generator` is sufficient regardless of how many exchange-sim users are placing orders simultaneously. The DashMap price cache decouples the update frequency (100/sec) from the read frequency (one per order).

**Maintainability:** The `ws_client.rs` module is self-contained — it owns the reconnection logic and the cache update. If the fpga service URL changes, only `ws_client.rs` and the config need updating. The rest of the codebase reads from `AppState.prices` and is unaware of how prices arrive.

**Alternatives considered:** HTTP polling (`GET /api/v1/tick/{symbol}` every N ms) — introduces artificial latency, rate-limit exposure, and unnecessary HTTP overhead. A shared Redis pub/sub channel — adds an infrastructure dependency for what is already a solved problem with a direct WebSocket.

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

## Security

### Authentication Flow

Every request to a protected endpoint passes through `extract_claims()` before any business logic runs:

```
HTTP request
  └─ extract_claims(req)
       ├─ read Authorization: Bearer <token> header → 401 if absent
       ├─ jsonwebtoken::decode(token, &JWT_SECRET, &Validation)
       │    ├─ verify HMAC-SHA256 signature          → 401 if invalid
       │    └─ verify exp claim (token expiry)       → 401 if expired
       └─ check Claims.jti in blocklist DashMap      → 401 if revoked
            └─ return Claims { user_id, role, jti }
```

**JWT secret:** `JWT_SECRET` is read from the environment variable `JWT_SECRET`. It must be set before deployment. The service does not start with a default secret — an absent or empty value causes a startup panic, preventing silent production operation with a known-weak key.

**Token contents:** The JWT payload (`Claims`) encodes `user_id` (UUID), `role` (`trader` / `admin`), `jti` (UUID, unique per token), and `exp` (Unix timestamp). It does not encode the password, balance, or any mutable state. All mutable state is read from ClickHouse on demand.

**Token revocation:** `POST /auth/logout` inserts the token's `jti` into `AppState.blocklist: DashMap<String, i64>` (jti → expiry timestamp). A background task running every 5 minutes removes entries whose expiry has passed, bounding memory growth. This provides immediate token invalidation on logout without a database roundtrip on every request.

**Password hashing:** Passwords are hashed with `bcrypt` before storage. The bcrypt work factor encodes the cost in the hash string — the stored hash is never a raw or reversible representation of the password. `bcrypt::verify` uses constant-time comparison to prevent timing attacks that could leak information about correct password prefixes.

---

### Authorization — Role Model

Two roles exist: `trader` (default) and `admin`. The role is embedded in the JWT at login time and re-verified on every request — it is never read from the database after the token is issued.

**Role enforcement per endpoint:**

| Endpoint group | Who can access | Enforcement |
|---|---|---|
| `POST /auth/register`, `POST /auth/login` | Anyone (no token required) | No `extract_claims()` call |
| `GET /health`, `GET /symbols`, `GET /prices` | Anyone | No `extract_claims()` call |
| `POST /auth/logout` | Authenticated users | `extract_claims()` required; any valid role |
| `GET /account`, `POST /orders`, `GET /orders`, `DELETE /orders/{id}` | Authenticated traders and admins | `extract_claims()` required; any valid role |
| `GET /admin/*`, `POST /admin/*`, `PATCH /admin/*` | Admin only | `extract_claims()` + `require_admin(claims)` |

**`require_admin` check:**
```rust
fn require_admin(claims: &Claims) -> Result<(), AppError> {
    if claims.role == UserRole::Admin {
        Ok(())
    } else {
        Err(AppError::Forbidden("Admin role required"))
    }
}
```

Admin routes return `403 Forbidden` if the `role` claim is `trader`. A trader cannot escalate privileges by modifying the JWT — any tampering invalidates the HMAC signature and causes a `401` at the `extract_claims()` step.

**User isolation:** All data queries are scoped by `user_id` extracted from the verified JWT claims — not from a query parameter or request body the client controls. A trader cannot access another trader's orders or balance by changing a URL parameter.

---

### Sensitive Data Protection

**Passwords:** Never stored in plaintext. Only the bcrypt hash is written to ClickHouse. The hash is never returned in any API response.

**Balances and orders:** Stored in ClickHouse under the `exchange` database. The `exchange_user` ClickHouse role has `SELECT` and `INSERT` only on `exchange.*` — it cannot `DROP`, `ALTER`, or read `system.*` tables.

**JWT secret:** Stored only in the environment — not in the codebase, not in ClickHouse. Rotated by changing the env var and restarting the service (existing tokens become invalid immediately since they were signed with the old key).

**ClickHouse credentials:** `CLICKHOUSE_USER` and `CLICKHOUSE_PASSWORD` are environment variables. The service logs a startup warning if the defaults (`inserter_user` / `inserter_pass`) are detected, preventing silent production operation with default credentials.

**No sensitive data in logs:** `tracing` log statements record `user_id`, `order_id`, and `symbol` — never passwords, token values, or balance amounts.

---

### Secure Communication

**Frontend → exchange-sim:** In production, the browser communicates with Vercel over HTTPS. Vercel rewrites `/api/*` to the exchange-sim. The exchange-sim itself runs plain HTTP; TLS termination happens at Vercel's edge. All bearer tokens travel over encrypted connections in production.

**exchange-sim → fpga-hft-data-generator:** The WebSocket price feed connection runs on the internal network (Docker Compose network or private VPC). No authentication is required for this connection — the price feed is read-only synthetic data with no user-sensitive content. In production, this connection should be restricted to private network access rather than exposed to the internet.

**exchange-sim → ClickHouse:** Plain HTTP on the internal network (Docker Compose). Credentials are passed as HTTP Basic auth headers. For production, ClickHouse should be on a private network segment with TLS enabled on the HTTP interface.

**Input validation:** All database queries use parameterized bindings (`client.query("... WHERE id = ?").bind(id)`). There is no string interpolation of user-provided values into SQL — ClickHouse injection is not possible through the API layer.

---

### Security Gaps (Production Hardening Required)

| Gap | Risk | Recommended fix |
|---|---|---|
| No TLS on exchange-sim | Tokens travel in plaintext on internal network | TLS termination at nginx/envoy, or Rustls in-process |
| Token blocklist lost on restart | Revoked tokens valid again until expiry | Back blocklist with Redis |
| No per-user rate limiting | Unlimited orders per second per user | Per-user sliding window counter in Redis |
| CORS fully open | Any origin can make credentialed requests | Restrict `allow_origin` to Vercel app domain |
| No IP-based order rate limit | Brute-force order spam from one IP | Extend `actix-governor` to cover order endpoints separately |
| ClickHouse plain HTTP | Credentials and queries in plaintext on network | Enable ClickHouse TLS interface; restrict to private network |

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

---

## ClickHouse Database Design

### Overview

Two ClickHouse databases serve the full system. `hft_dashboard` is owned by `fpga-hft-data-generator` and holds all market data. `exchange` is owned by `exchange-sim` and holds all account state. `exchange-sim` also holds a cross-database `SELECT` grant on `hft_dashboard` so it can read live prices directly.

The authoritative schema is in `clickhouse-config/init.sql`. The `fpga-hft-data-generator/schema/schema.sql` file is a developer-facing subset used for standalone bringup without Docker Compose.

---

### Databases and Ownership

| Database | Owner service | Tables |
|---|---|---|
| `hft_dashboard` | `fpga-hft-data-generator` | `historical_trades`, `market_ohlc`, 5 materialized views |
| `exchange` | `exchange-sim` | `users`, `orders`, `positions` |

ClickHouse users:
- `inserter_user` — INSERT + SELECT on `hft_dashboard.*`
- `exchange_user` — INSERT + SELECT on `exchange.*`, SELECT on `hft_dashboard.*`

---

### Table Reference

#### `hft_dashboard.historical_trades`

Raw trade ticks written by `fpga-hft-data-generator` at up to 100 ticks/sec per symbol.

```sql
ENGINE = MergeTree()
ORDER BY (symbol, timestamp)
PARTITION BY toYYYYMM(timestamp)
TTL toDateTime(timestamp) + INTERVAL 2 HOUR DELETE
```

| Column | Type | Codec | Notes |
|---|---|---|---|
| `symbol` | String | ZSTD(3) | Trading pair, e.g. `SOL/USDC` |
| `side` | Int8 | — | 1 = buy, 2 = sell |
| `price` | Decimal64(8) | ZSTD(3) | 8 decimal places |
| `amount` | Decimal64(8) | ZSTD(3) | 8 decimal places |
| `timestamp` | DateTime64(6) | DoubleDelta + ZSTD(1) | Microsecond UTC |
| `order_id` | String | ZSTD(3) | UUID |
| `trader_id` | UInt32 | ZSTD(1) | Simulated trader identity |

**Design rationale:**
- `ORDER BY (symbol, timestamp)` — all time-range queries filter on `symbol` first. ClickHouse physically sorts data by this key, so a `WHERE symbol = 'SOL/USDC' AND timestamp >= ...` scan skips all granules that don't match — no full table scan.
- `PARTITION BY toYYYYMM(timestamp)` — monthly partitions bound the scan surface for time-bounded queries and allow `ALTER TABLE DROP PARTITION` for bulk data removal without rewriting the table.
- `DoubleDelta` codec on `timestamp` — optimal for monotonically increasing integers with regular spacing (HFT tick data). Compresses sequential microsecond timestamps by ~80% vs raw storage.
- `ZSTD(3)` on price/amount/symbol — ZSTD at level 3 favours compression ratio; suitable for columns written in large batches rather than read on the critical path.
- TTL 2 hours — intentionally short for this synthetic simulator. At 100 ticks/sec × 2 symbols the table would grow ~60 million rows/hour; the 2-hour window keeps storage bounded while still supporting 1m chart backfill.

---

#### `hft_dashboard.market_ohlc`

Pre-aggregated 1-minute OHLCV candles written by `fpga-hft-data-generator` on candle close.

```sql
ENGINE = ReplacingMergeTree()
ORDER BY (symbol, candle_time)
PARTITION BY toYYYYMM(candle_time)
TTL toDateTime(candle_time) + INTERVAL 90 DAY DELETE
```

| Column | Type | Codec | Notes |
|---|---|---|---|
| `symbol` | String | ZSTD(3) | |
| `candle_time` | DateTime64(6) | DoubleDelta + ZSTD(1) | Minute boundary (UTC) |
| `open` | Decimal64(8) | ZSTD(3) | First price in candle |
| `high` | Decimal64(8) | ZSTD(3) | |
| `low` | Decimal64(8) | ZSTD(3) | |
| `close` | Decimal64(8) | ZSTD(3) | Last price in candle |
| `volume` | Decimal64(8) | ZSTD(3) | Sum of `amount` in candle |
| `change_1h` | Decimal64(8) | ZSTD(3) | % change vs price 1h ago |
| `change_24h` | Decimal64(8) | ZSTD(3) | % change vs price 24h ago |

**Design rationale:**
- `ReplacingMergeTree()` — ClickHouse has no UPDATE. Candle values for an open (in-progress) minute change with every tick. Re-inserting a new row with the same `(symbol, candle_time)` key and reading with `FINAL` forces deduplication, providing upsert semantics without a transactional database.
- 90-day TTL — chart data beyond 3 months is rarely needed for a trading dashboard. Longer retention would require either larger monthly partitions or a separate cold-storage table.

---

#### `hft_dashboard.historical_trades_mv_*` (Materialized Views)

Five `AggregatingMergeTree` views that incrementally maintain OHLCV aggregates at each resolution. Populated automatically on every INSERT to `historical_trades`.

| View | Interval function | Typical query window |
|---|---|---|
| `historical_trades_mv_1m` | `toStartOfMinute` | 24 hours |
| `historical_trades_mv_5m` | `toStartOfInterval(..., INTERVAL 5 MINUTE)` | 5 days |
| `historical_trades_mv_15m` | `toStartOfInterval(..., INTERVAL 15 MINUTE)` | 15 days |
| `historical_trades_mv_1h` | `toStartOfHour` | 30 days |
| `historical_trades_mv_1d` | `toStartOfDay` | 3 months |

Each view stores partial aggregate states:

| Aggregate state | Resolves to | Query function |
|---|---|---|
| `argMinState(price, timestamp)` | First price (open) | `argMinMerge(open)` |
| `maxState(price)` | Highest price (high) | `maxMerge(high)` |
| `minState(price)` | Lowest price (low) | `minMerge(low)` |
| `argMaxState(price, timestamp)` | Last price (close) | `argMaxMerge(close)` |
| `sumState(amount)` | Total volume | `sumMerge(volume)` |

**Design rationale:**
- Without MVs, every chart load would `GROUP BY toStartOfInterval(timestamp, ...)` across potentially millions of raw tick rows. At 100 ticks/sec × 2 symbols the 1h view alone covers ~720,000 raw rows. The MV reduces this to a `GROUP BY` across ~60 pre-merged candles.
- `AggregatingMergeTree` stores binary intermediate state (not final values). Multiple inserts in the same candle window are merged in the background — the `*Merge` functions at query time finalise the state into usable values.
- Backfill: MVs only capture new inserts from creation time. The SQL file (`db/queries/materialized_views_multi.sql`) includes commented INSERT...SELECT backfill queries that re-aggregate existing `historical_trades` data — run these once on first deploy.

---

#### `exchange.users`

```sql
ENGINE = ReplacingMergeTree(created_at)
ORDER BY id
```

| Column | Type | Notes |
|---|---|---|
| `id` | String | UUID |
| `username` | String | Uniqueness enforced in-process (Mutex), not at DB level |
| `password_hash` | String | bcrypt |
| `role` | String | `'admin'` / `'trader'` |
| `balance_usdc` | String | Stored as decimal string to avoid float precision loss |
| `created_at` | DateTime64(6) | Version column — higher value wins on merge |
| `is_active` | UInt8 | Soft-delete flag |

**Design rationale:**
- `ReplacingMergeTree(created_at)` — balance updates re-insert a new row with a later `created_at`. The higher timestamp wins at merge time. All reads use `FINAL` to force dedup before background merges complete.
- `balance_usdc` as String — ClickHouse stores Decimal, but the exchange-sim serialises balances as decimal strings to avoid f64 rounding. Rust `rust_decimal::Decimal` parses the string at read time.
- No partition — the users table will have at most thousands of rows. Partitioning at this scale adds overhead with no benefit.

---

#### `exchange.orders`

```sql
ENGINE = ReplacingMergeTree(updated_at)
ORDER BY (user_id, id)
PARTITION BY toYYYYMM(created_at)
```

| Column | Type | Notes |
|---|---|---|
| `id` | String | UUID |
| `user_id` | String | FK to `exchange.users.id` |
| `symbol` | String | `'SOL/USDC'` etc. |
| `side` | String | `'buy'` / `'sell'` |
| `order_type` | String | `'market'` / `'limit'` |
| `price` | String | Fill price; `'0'` for pending limit orders |
| `amount` | String | Quantity in base asset |
| `total_usdc` | String | `price × amount` |
| `limit_price` | String | Empty for market orders |
| `status` | String | `'filled'` / `'rejected'` / `'pending'` / `'canceled'` |
| `reject_reason` | String | Empty when not rejected |
| `realized_pnl` | String | Empty for buys and pending orders |
| `created_at` | DateTime64(6) | Immutable — partition key |
| `updated_at` | DateTime64(6) | Version column |

**Design rationale:**
- `ReplacingMergeTree(updated_at)` — limit orders transition through states (`pending` → `filled` or `canceled`). Each transition re-inserts the row with a newer `updated_at`; `FINAL` at read time returns only the latest state.
- `ORDER BY (user_id, id)` — the most common read pattern is `WHERE user_id = ?`. Placing `user_id` first in the sort key makes per-user order history scans read a contiguous range of sorted data.
- `PARTITION BY toYYYYMM(created_at)` — monthly partitions bound the scan for time-filtered admin queries and enable partition-level archiving.

---

#### `exchange.positions`

```sql
ENGINE = ReplacingMergeTree(updated_at)
ORDER BY (user_id, symbol)
```

| Column | Type | Notes |
|---|---|---|
| `user_id` | String | |
| `symbol` | String | |
| `quantity` | String | Base asset held; `'0'` means no position |
| `avg_buy_price` | String | Weighted average cost basis |
| `updated_at` | DateTime64(6) | Version column |

**Design rationale:**
- `ORDER BY (user_id, symbol)` — position queries always filter on `user_id`. Positions are few per user (one per symbol) so no partition is needed.
- Non-zero filter: reads use `WHERE toFloat64OrZero(quantity) > 0` to exclude closed (zeroed) positions from the result set without physically deleting rows.

---

### DB Interactions — Cross-Service Summary

| Operation | Service | Table | Pattern |
|---|---|---|---|
| Tick INSERT (batched) | fpga-hft-data-generator | `hft_dashboard.historical_trades` | Buffered, flush every 1s or 1000 rows |
| Candle INSERT | fpga-hft-data-generator | `hft_dashboard.market_ohlc` | On minute boundary, per symbol |
| MV population | ClickHouse (automatic) | `historical_trades_mv_*` | Triggered by every INSERT to `historical_trades` |
| 1h/24h change poll | fpga-hft-data-generator | `hft_dashboard.historical_trades` | `SELECT price ... ORDER BY timestamp ASC LIMIT 1` every 5s |
| OHLCV read | fpga-hft-data-generator | `hft_dashboard.market_ohlc` / MVs | On `GET /api/v1/ohlcv/{symbol}?interval=` |
| Candle backfill | fpga-hft-data-generator | `hft_dashboard.market_ohlc` | INSERT...SELECT on startup |
| User register | exchange-sim | `exchange.users` | INSERT new row |
| User login | exchange-sim | `exchange.users` | SELECT FINAL WHERE username = ? |
| Balance update | exchange-sim | `exchange.users` | Re-INSERT with new `balance_usdc` + later `created_at` |
| Order place | exchange-sim | `exchange.orders` | INSERT with `status='filled'` (market) or `'pending'` (limit) |
| Order fill (limit) | exchange-sim | `exchange.orders` | Re-INSERT with `status='filled'`, exec price, `updated_at=now()` |
| Order cancel | exchange-sim | `exchange.orders` | Re-INSERT with `status='canceled'` |
| Order history | exchange-sim | `exchange.orders` | SELECT FINAL WHERE user_id = ? ORDER BY created_at DESC LIMIT N |
| Position upsert | exchange-sim | `exchange.positions` | Re-INSERT with new quantity/avg_price |
| Position read | exchange-sim | `exchange.positions` | SELECT FINAL WHERE user_id = ? AND quantity > 0 |

---

### Query Performance Notes

#### Design choices that prevent slow queries at scale

**1. ORDER BY prefix alignment**
Every production query filters on the leading column(s) of the ORDER BY key before applying any other predicate. Violating this (e.g. `WHERE symbol = ?` on a table whose key starts with `timestamp`) forces a full-table scan across all granules.

| Table | ORDER BY | Common filter — aligned? |
|---|---|---|
| `historical_trades` | `(symbol, timestamp)` | `WHERE symbol = ? AND timestamp >= ?` — yes |
| `market_ohlc` | `(symbol, candle_time)` | `WHERE symbol = ? AND candle_time >= ?` — yes |
| `exchange.orders` | `(user_id, id)` | `WHERE user_id = ?` — yes |
| `exchange.positions` | `(user_id, symbol)` | `WHERE user_id = ?` — yes |

**2. FINAL keyword — cost and when to use it**
`FINAL` forces read-time deduplication on `ReplacingMergeTree` tables. Without it, stale duplicate rows may be returned before background merges run.

Cost: ClickHouse reads all versions of each row, compares version columns, and discards losers. On a table with millions of rows and high re-insert rate this can be 2–5× slower than a plain SELECT.

Mitigation strategies for large deployments:
- `OPTIMIZE TABLE exchange.orders FINAL` — forces a merge, making subsequent SELECTs cheaper. Run during low-traffic windows.
- Point lookups (`WHERE id = ? LIMIT 1`) have low FINAL overhead because only a few granules are read.
- For admin full-table scans (`list_all_orders`) on large datasets, consider adding a `created_at` range filter to limit the partition scan before FINAL deduplication runs.

**3. Materialized views eliminate GROUP BY on raw ticks**
The 5 OHLCV MVs replace on-the-fly aggregation. Approximate query cost comparison at scale (2 symbols × 30 days × 100 ticks/sec):

| Approach | Rows scanned for 1h of 5m candles |
|---|---|
| GROUP BY on `historical_trades` | ~720,000 raw tick rows |
| SELECT from `historical_trades_mv_5m` | ~24 pre-merged candle rows |

The MV fallback (on-the-fly GROUP BY) is used only when the MV is empty — during the first minutes after deployment before backfill runs.

**4. Batched inserts**
`fpga-hft-data-generator` buffers trade rows and flushes in batches (1,000 rows or 1 second, whichever comes first). ClickHouse is optimised for bulk inserts; single-row inserts at 100/sec would create ~360,000 small parts per hour, overwhelming the background merger and eventually causing `Too many parts` errors.

**5. Partition pruning**
Monthly partitions on `historical_trades` and `exchange.orders` mean queries with a `timestamp >= now() - INTERVAL N MINUTE` condition only open the current (and possibly previous) month's partition files. For the TTL-bounded `historical_trades` table (2-hour window), the entire active dataset fits in one partition.

**6. Potential optimizations for future scale**

| Scenario | Optimization |
|---|---|
| `exchange.users FINAL` slow with many balance updates | Add `OPTIMIZE TABLE exchange.users FINAL` to a nightly cron |
| `list_all_orders` admin scan slow | Add `WHERE created_at >= ?` filter; expose `from_date` query param |
| `historical_trades` hot reads | Add a `skip index` on `price` (`minmax`, granularity 4) for range queries |
| `positions` query slow under many symbols | Already ORDER BY `(user_id, symbol)` — add PREWHERE on `user_id` |
| MV query slow after high-volume insert burst | Run `OPTIMIZE TABLE historical_trades_mv_5m FINAL` to force merge of pending parts |
| Tick volume grows beyond 100/sec | Switch `historical_trades` to `PARTITION BY toYYYYMMDD(timestamp)` for finer partition pruning |
