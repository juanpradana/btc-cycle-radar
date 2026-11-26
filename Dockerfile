# syntax=docker/dockerfile:1
# ---------------------------------------------------------------------------
# BTC Cycle Radar — image produksi multi-stage.
#
#   Stage 1 "builder"  : compile binary release Rust dari berkas sumber.
#   Stage 2 "runtime"  : hanya binary + ca-certificates, dijalankan non-root.
#
# Catatan desain:
#   * TLS memakai rustls (lihat backend/Cargo.toml), jadi image ini TIDAK
#     membutuhkan OpenSSL/libssl di builder maupun runtime.
#   * Skema SQL di-embed saat compile via include_str!("../migrations/..."),
#     sehingga build tidak memerlukan DATABASE_URL.
#   * Kredensial HANYA masuk lewat environment variable saat runtime —
#     tidak pernah ditulis ke dalam image.
# ---------------------------------------------------------------------------

# ------------------------------- Stage 1 ----------------------------------
FROM rust:1.100-bookworm AS builder

WORKDIR /build

# Manifest disalin lebih dulu agar lapisan dependency bisa dipakai ulang
# ketika hanya kode sumber yang berubah.
COPY backend/Cargo.toml backend/Cargo.lock ./
COPY backend/migrations ./migrations
COPY backend/src ./src

# --locked  : pakai Cargo.lock persis (reproducible / tidak melompat versi).
# Cache mount BuildKit: registry & artefak target dipertahankan antar-build,
# jadi rebuild berikutnya tidak mengulang kompilasi seluruh dependency.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --locked --bin btc-cycle-radar \
 && install -Dm0755 /build/target/release/btc-cycle-radar /out/btc-cycle-radar

# ------------------------------- Stage 2 ----------------------------------
FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="btc-cycle-radar" \
      org.opencontainers.image.description="Dashboard intelijen & valuasi siklus panjang Bitcoin (Rust axum + SQLite)" \
      org.opencontainers.image.source="https://github.com/juanpradana/btc-cycle-radar" \
      org.opencontainers.image.licenses="MIT"

# ca-certificates : verifikasi TLS ke Binance / CoinMetrics / BGeometrics / Yahoo.
# curl            : dipakai HEALTHCHECK (tanpa ini healthcheck gagal jalan).
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl \
 && rm -rf /var/lib/apt/lists/*

# Jalankan sebagai non-root. /app/data adalah mount point volume SQLite;
# direktori ini di-chown di image supaya volume baru mewarisi kepemilikannya.
RUN groupadd --gid 10001 app \
 && useradd  --uid 10001 --gid 10001 --no-create-home --shell /usr/sbin/nologin app \
 && mkdir -p /app/data /app/static \
 && chown -R 10001:10001 /app

WORKDIR /app

COPY --from=builder --chown=10001:10001 /out/btc-cycle-radar /usr/local/bin/btc-cycle-radar
COPY --chown=10001:10001 frontend/ /app/static/

# Nilai default; bisa dioverride lewat compose / `docker run -e`.
# DATABASE_URL relatif terhadap WORKDIR (/app) -> file di /app/data.
ENV BIND_ADDR=0.0.0.0:8099 \
    DATABASE_URL=sqlite://data/btc_cycle_radar.db \
    BOOTSTRAP_START=2010-07-01 \
    MIN_REQUIRED_CANDLES=4500 \
    AUTO_BOOTSTRAP=true \
    STATIC_DIR=/app/static \
    RUST_LOG=btc_cycle_radar=info,sqlx=warn,axum=info,tower_http=info

EXPOSE 8099

# Bootstrap awal (unduh riwayat 2010 -> sekarang) selesai SEBELUM listener bind,
# karena itu start-period diberi ruang lebar agar container tidak dianggap gagal.
HEALTHCHECK --interval=30s --timeout=5s --start-period=600s --retries=5 \
    CMD curl -fsS http://127.0.0.1:8099/healthz || exit 1

USER 10001

# Tanpa argumen = mode server. Subcommand CLI tetap bisa dipanggil, mis.:
#   docker run --rm -v btc-data:/app/data btc-cycle-radar:latest sync --incremental
CMD ["btc-cycle-radar"]
