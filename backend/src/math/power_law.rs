//! Power Law model (Giovanni Santostasi).
//!
//! Fair value: `P_fair = 10^(-17.016) * d^5.845`
//! dengan `d` = jumlah hari sejak Genesis Block (2009-01-03, UTC).
//!
//! Band model:
//! - Support / Floor  = 0.42 x Fair Value (regresi kuantil bawah historis)
//! - Fair Value       = garis tengah
//! - Top / Resistance = 5.0 x Fair Value (batas atas kanal historis)
use chrono::NaiveDate;

/// Tanggal Genesis Block Bitcoin (blok #0), UTC.
pub const GENESIS_DATE: (i32, u32, u32) = (2009, 1, 3);

/// Eksponen & intersep regresi power law (Santostasi, log10 space).
pub const PL_EXPONENT: f64 = 5.845;
pub const PL_INTERCEPT: f64 = -17.016;

/// Pengali support (floor historis).
pub const SUPPORT_MULTIPLIER: f64 = 0.42;
/// Pengali top (batas atas kanal historis).
pub const TOP_MULTIPLIER: f64 = 5.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PowerLawModel {
    /// Hari sejak genesis (d).
    pub days: i64,
    /// Fair value hasil regresi.
    pub fair_price: f64,
    /// Support / floor (0.42 x fair).
    pub support_price: f64,
    /// Top / resistance (5.0 x fair).
    pub top_price: f64,
}

/// Tanggal genesis sebagai `NaiveDate`.
///
/// Konstanta literal tanggal valid, sehingga `expect` di sini tidak bisa gagal
/// pada runtime; tetap didokumentasikan agar tidak ada panic path tersembunyi.
pub fn genesis_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(GENESIS_DATE.0, GENESIS_DATE.1, GENESIS_DATE.2)
        .expect("GENESIS_DATE adalah tanggal valid (konstanta compile-time)")
}

/// Jumlah hari sejak genesis. Negatif bila `date` sebelum genesis.
pub fn days_since_genesis(date: NaiveDate) -> i64 {
    (date - genesis_date()).num_days()
}

/// Nilai power law pada hari ke-`d`.
///
/// Mengembalikan `None` bila `d <= 0` (tidak terdefinisi untuk logaritma).
pub fn calculate_at_day(d: i64) -> PowerLawModel {
    // d minimal 1 agar tidak menghasilkan 0/NaN; untuk d<=0 kita clamp ke 1.
    let d_eff = d.max(1) as f64;
    let log10_fair = PL_INTERCEPT + PL_EXPONENT * d_eff.log10();
    let fair = 10f64.powf(log10_fair);

    PowerLawModel {
        days: d,
        fair_price: fair,
        support_price: fair * SUPPORT_MULTIPLIER,
        top_price: fair * TOP_MULTIPLIER,
    }
}

/// Power law pada tanggal tertentu.
pub fn calculate_at_date(date: NaiveDate) -> PowerLawModel {
    calculate_at_day(days_since_genesis(date))
}

/// Evaluasi posisi harga terhadap kanal power law.
///
/// - `<= support`            → `DEEP_BOTTOM`
/// - `<= support * 1.15`     → `BUYZONE`
/// - `>= top`                → `TOP_ZONE`
/// - selain itu              → `NEUTRAL`
pub fn evaluate_power_law_status(price: f64, model: &PowerLawModel) -> &'static str {
    if price <= model.support_price {
        "DEEP_BOTTOM"
    } else if price <= model.support_price * 1.15 {
        "BUYZONE"
    } else if price >= model.top_price {
        "TOP_ZONE"
    } else {
        "NEUTRAL"
    }
}

/// Jarak persentase harga spot ke suatu level (positif = level di atas spot).
pub fn distance_pct(price: f64, level: f64) -> Option<f64> {
    if price <= 0.0 || !price.is_finite() || !level.is_finite() {
        return None;
    }
    Some((level - price) / price * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn genesis_is_day_zero() {
        assert_eq!(days_since_genesis(genesis_date()), 0);
    }

    #[test]
    fn fair_value_is_finite_and_positive() {
        for d in [1i64, 100, 1000, 5000, 6500] {
            let m = calculate_at_day(d);
            assert!(m.fair_price.is_finite() && m.fair_price > 0.0, "d={d}");
            assert!(m.support_price < m.fair_price);
            assert!(m.top_price > m.fair_price);
        }
    }
}
