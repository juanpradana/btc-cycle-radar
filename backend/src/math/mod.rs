//! Math Engine — seluruh kalkulasi finansial murni (tanpa I/O jaringan).
//!
//! Prinsip:
//! - Fungsi murni (pure), deterministik, mudah di-unit-test.
//! - Tidak ada `unwrap()` pada input eksternal; semua kegagalan → `Option`/`Result`.
//! - Setiap rumus divalidasi terhadap nilai referensi di `tests/math_engine_tests.rs`.

pub mod ahr999;
pub mod clock;
pub mod ma;
pub mod power_law;
pub mod quantile;
pub mod rainbow;
