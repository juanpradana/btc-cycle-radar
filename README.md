# BTC Cycle Radar — Bitcoin Long-Cycle Intelligence & Valuation Dashboard

> **Production-ready full-stack** (Rust + React/TypeScript) untuk memantau seluruh indikator Bottom, Top, Buyzone, dan Risk-Off/Exit Bitcoin dalam siklus jangka panjang.

## Status Phase 1
- ✅ Schema SQLite (6 tabel) + migrasi `sqlx`
- ✅ Bootstrap historis **CoinMetrics PriceUSD 5,974 daily rows (2010-07-18 → hari ini)**
- ✅ Update harian Binance klines (2017-08-17 → hari ini) + spot price
- ✅ Unit tests parser + integration tests schema/migrations
- ✅ CLI: `bootstrap [--force|--verify]`, `update [days]`, `migrate`
- ✅ HTTP API: `/healthz`, `/api/v1/meta`, `/api/v1/bootstrap`, `/api/v1/update`

---

## Quick Start (Windows)

```bash
# 1. Prereq: Rust (GNU toolchain) + MinGW-w64 (gcc)
#    Jika belum punya, gunakan toolchain di D:\Hermes\toolchains\mingw64
#    atau install via rustup (stable-x86_64-pc-windows-gnu) + WinLibs GCC.

# 2. Build & test
cd backend
export PATH="$HOME/.cargo/bin:/d/Hermes/toolchains/mingw64/bin:$PATH"  # sesuaikan
cargo test --color never

# 3. Run server (auto-bootstrap)
cargo run --color never
# → listening on http://0.0.0.0:8099

# 4. Verify candle count via CLI
cargo run --color never -- bootstrap --verify
```

---

## CLI Commands

| Perintah | Deskripsi |
|---|---|
| `cargo run -- bootstrap` | Jalankan bootstrap historis penuh (idempotent) |
| `cargo run -- bootstrap --force` | Paksa fetch ulang (abaikan cache) |
| `cargo run -- bootstrap --verify` | Hanya verifikasi `candles >= 4500` (exit code 0/1) |
| `cargo run -- update [days]` | Update klines & spot price N hari terakhir (default 5) |
| `cargo run -- migrate` | Jalankan migrasi saja |

---

## API Endpoints (Phase 1)

```
GET  /healthz                  → {status, version}
GET  /api/v1/meta              → candle_count, first_date, last_date, bootstrap_ok
GET  /api/v1/bootstrap         → trigger bootstrap manual (force=true)
GET  /api/v1/update            → trigger update manual (5 days)
```

---

## Data Sources (100% Gratis)

| Sumber | Metrik | Cakupan |
|---|---|---|
| **CoinMetrics Community** | `PriceUSD` daily | 2010-07-18 → hari ini (5,974 rows) |
| **Binance (data-api.binance.vision)** | OHLCV daily + spot | 2017-08-17 → hari ini |
| **BGeometrics (free tier)** | NUPL, True Market Mean, AVIV, LTH Realized Price, STH MVRV, % Supply in Profit | 2022-11 → hari ini (delayed ~7 hari) |
| **Yahoo Finance** | Gold (XAU), S&P 500, DXY | 5 tahun |
| **Checkonchain/derivasi** | AHR999, Power Law, Pi Cycle, dll | dihitung dari OHLCV |

> **No mock/hardcoded data.** Jika sumber gagal, backend mengembalikan data terakhir dari SQLite + flag `is_stale`.

### Kebijakan Indikator Missing (final, 2026-11-26)

**Status: 18/19 indikator dievaluasi, `lth_realized_pl_ratio` dinonaktifkan permanen.**

Dua indikator yang sebelumnya ditandai MISSING dievaluasi ulang:

| Indikator | Endpoint BGeometrics | Status & Transform |
|---|---|---|
| `supply_in_profit` | `GET /v1/profit-loss` | **Aktif**: `profitLoss` fraksi 0–1 → dikali 100 menjadi persen. Nilai `>= 1.0` dibuang (artefak pinned provider). |
| `lth_realized_pl_ratio` | *tidak ada* | **DINONAKTIFKAN**: lihat penjelasan di bawah. |

**`lth_realized_pl_ratio` DINONAKTIFKAN (2026-11-26):** penyebut `realized_loss_lth`
dari tier gratis BGeometrics kolaps saat bull market — median harian hanya
**$2.090 di 2025** (241 dari 361 hari < $100k). Akibatnya rasio `profit/loss`
meledak ke 1e8–1e11 dan **selalu memicu `> 20 = TopZone`** di engine: sinyal palsu
permanen. Tidak ada endpoint rasio-LTH gratis di provider mana pun. Glassnode
menyediakan rasio ini (`ema(LTH Realized Profit / LTH Realized Loss, 7)`) hanya
di tier **Professional** dengan metrik *entity-adjusted*.

Kebijakan proyek: indikator yang tidak dapat dihitung dari data layak dilaporkan
**MISSING**, bukan diisi angka menyesatkan. Engine mengembalikan
`missing_indicators` berisi `lth_realized_pl_ratio`, bobotnya di-redistribusi.
Flag kebijakan: `LTH_REALIZED_PL_RATIO_ENABLED = false` (teruji).

**Guard rail kualitas data (wajib, sudah diuji):**

1. **`profit-loss` plausibilitas** — nilai `>= 1.0` (100%) atau `<= 0.0` **dibuang**,
   bukan dicap 100%. Alasan: supply in profit tidak pernah tepat 100% (selalu ada
   UTXO di bawah harga akuisisi), dan BGeometrics mengirim `1.0` secara *pinned*
   pada rentang historis panjang (artefak provider). Cap akan berarti mengarang nilai.
   Konsekuensi jujur: deret historis indikator ini tipis — UI menampilkan provenance
   `latest_date` + `staleness_days` agar keterbatasan terlihat.
2. **`lth_realized_pl_ratio` degenerasi penyebut** — baris dengan `loss < $1.000`
   dibuang (rasio meledak, mis. 1.99e11, saat kerugian mendekati nol); profit `<= 0`
   atau non-finite juga dibuang. Rasio disimpan *raw* (tanpa EMA7) agar deterministik;
   provenance `source = "bgeometrics_derived"`.
3. **Provider delay disclosure** — `BGEOMETRICS_FREE_TIER_DELAY_DAYS = 7`. Loader
   menambahkan warning eksplisit saat data on-chain ≥ 5 hari basi.

**Prinsip:** indikator yang datanya tidak lolos guard dilaporkan **missing secara
jujur** untuk tanggal tersebut — tidak ada nilai perkiraan, tidak ada interpolasi.

### Definisi Autoritatif (diverifikasi dari dokumentasi primer, 2026-11-26)

**Percent Supply in Profit** — rumus resmi:
`Percent Supply in Profit = 100 × (Supply in Profit) / (Total Supply)` — Glassnode,
*metric guide* [percent-supply-in-profit](https://docs.glassnode.com/further-information/metric-guides/profit-loss-supply/percent-supply-in-profit);
CryptoQuant [profit-and-loss-supply](https://docs.cryptoquant.com/) menulis
`Supply in Profit = Σ_u value_u · I(price_u > price_created,u)`.
→ BGeometrics `/v1/profit-loss` mendefinisikan *"% of BTC supply currently in profit"*
(OpenAPI `getProfitLoss`), jadi pemetaan kita **eksak**, bukan substitusi.

**LTH Realized P/L Ratio** — Glassnode:
`LTH Realized P/L Ratio = ema(LTH Realized Profit / LTH Realized Loss, 7)`
(studio.glassnode.com, chart `btc-lth-realized-plratio`). Kedua input adalah metrik
*entity-adjusted* (`realized_profit_lth_account_based`, `realized_loss_lth_account_based`)
yang hanya tersedia di tier **Professional** Glassnode.

> **Keputusan:** Tidak ada endpoint rasio-LTH gratis di provider mana pun.
> Upaya menurunkan rasio dari dua komponen BGeometrics (`/v1/realized_profit_lth` / `/v1/realized_loss_lth`)
> terbukti menghasilkan distorsi parah (penyebut kolaps ke ~$2.000 harian saat bull market sehingga rasio melonjak ke 1e8–1e11 dan memicu TopZone palsu).
> Oleh karena itu, metrik ini **dinonaktifkan secara permanen** di tier gratis (`LTH_REALIZED_PL_RATIO_ENABLED = false`),
> dilaporkan jujur sebagai **MISSING** pada payload API, dan bobot pilar 40% dialokasikan ulang secara dinamis ke indikator on-chain lainnya.
> Evaluasi siklus berjalan dengan 18/19 indikator aktif.

---

## Project Structure

```
btc-cycle-radar/
├── backend/
│   ├── Cargo.toml
│   ├── .cargo/config.toml        # GNU toolchain linker config
│   ├── .env.example
│   ├── migrations/0001_initial_schema.sql
│   ├── src/
│   │   ├── main.rs               # CLI + HTTP server
│   │   ├── lib.rs                # crate root
│   │   ├── config.rs             # AppConfig, structs
│   │   ├── db.rs                 # pool, migrations, meta
│   │   ├── market_data.rs        # CoinMetrics, Binance clients + store
│   │   ├── bootstrap.rs          # bootstrap_history, update_recent
│   │   └── api.rs                # axum routes
│   ├── tests/
│   │   ├── bootstrap_parsers.rs  # unit tests (no network)
│   │   ├── schema_migration.rs   # integration test migrations
│   │   └── live_bootstrap.rs     # #[ignore] real-network test
│   └── data/                     # SQLite file (gitignore)
└── frontend/                     # Phase 4: React + TS + Vite + Tailwind
```

---

## Next Phases

| Phase | Focus |
|---|---|
| **2** | Math engine (MA, Power Law, AHR999, Rainbow, Quantile, Clock), on-chain connectors (BGeometrics/CoinMetrics/Checkonchain), weighted ranking engine, `MarketRegimeSummary` |
| **3** | Client IP timezone endpoint `/api/v1/client-time` + in-memory cache |
| **4** | Frontend UI: Hero Zone Gauge, Checklist Terpenuhi, Interactive Chart (lightweight-charts), Indicator Ranking Table + Drawer edukatif, Price Ladder, Auto Theme (IP time) |
| **5** | Docker multi-stage, static frontend served by Rust, README deploy guide |

---

## Engineering Constraints (dipatuhi)

- Zero `unwrap()` pada path runtime HTTP/network
- Unit test untuk setiap kalkulasi finansial (math engine Phase 2)
- API key opsional via `.env` — sistem hidup penuh dengan OHLCV + CoinMetrics tanpa kunci
- SQLite WAL mode, busy_timeout ≥ 30s, FK on
- Rate-limit guard table `fetch_logs` untuk BGeometrics (max 8 endpoint/hari)

---

## License
MIT