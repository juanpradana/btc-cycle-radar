//! AHR999 Index.
//!
//! `AHR999 = (P / GeoMean_200d) * (P / PowerLawFit_d)`
//!
//! - `< 0.45`      → `DEEP_BOTTOM` (buyzone agresif)
//! - `0.45 – 1.2`  → `DCA_ZONE`
//! - `> 1.2`       → `WAIT_OVERHEATED`
use crate::math::power_law;

/// Batas bawah DCA zone.
pub const DEEP_BOTTOM_MAX: f64 = 0.45;
/// Batas atas DCA zone.
pub const DCA_ZONE_MAX: f64 = 1.2;

/// Geometric mean dari slice nilai positif.
///
/// Mengembalikan `None` bila slice kosong atau ada nilai <= 0 / non-finite.
pub fn geometric_mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut log_sum = 0.0;
    for v in values {
        if !v.is_finite() || *v <= 0.0 {
            return None;
        }
        log_sum += v.ln();
    }
    let mean_log = log_sum / values.len() as f64;
    let gm = mean_log.exp();
    if gm.is_finite() && gm > 0.0 {
        Some(gm)
    } else {
        None
    }
}

/// Geometric mean 200 hari terakhir dari seri harga close.
pub fn geomean_200d(closes: &[f64]) -> Option<f64> {
    if closes.len() < 200 {
        return None;
    }
    geometric_mean(&closes[closes.len() - 200..])
}

/// Hitung AHR999 untuk harga spot, geomean 200D, dan hari sejak genesis.
///
/// Mengembalikan `None` bila input tidak valid (harga/geomean <= 0, d <= 0).
pub fn calculate(spot_price: f64, geomean_200d: f64, days_since_genesis: i64) -> f64 {
    if !spot_price.is_finite() || spot_price <= 0.0 || !geomean_200d.is_finite() || geomean_200d <= 0.0 {
        return f64::NAN;
    }
    if days_since_genesis <= 0 {
        return f64::NAN;
    }
    let model = power_law::calculate_at_day(days_since_genesis);
    if !model.fair_price.is_finite() || model.fair_price <= 0.0 {
        return f64::NAN;
    }
    (spot_price / geomean_200d) * (spot_price / model.fair_price)
}

/// Klasifikasi nilai AHR999 ke status zona.
pub fn evaluate_status(ahr: f64) -> &'static str {
    if !ahr.is_finite() || ahr <= 0.0 {
        return "UNKNOWN";
    }
    if ahr < DEEP_BOTTOM_MAX {
        "DEEP_BOTTOM"
    } else if ahr <= DCA_ZONE_MAX {
        "DCA_ZONE"
    } else {
        "WAIT_OVERHEATED"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nan_inputs_return_nan() {
        assert!(calculate(f64::NAN, 100.0, 5000).is_nan());
        assert!(calculate(100.0, 0.0, 5000).is_nan());
        assert!(calculate(100.0, 100.0, 0).is_nan());
        assert_eq!(evaluate_status(f64::NAN), "UNKNOWN");
    }

    #[test]
    fn status_boundaries() {
        assert_eq!(evaluate_status(0.4499), "DEEP_BOTTOM");
        assert_eq!(evaluate_status(0.45), "DCA_ZONE");
        assert_eq!(evaluate_status(1.2), "DCA_ZONE");
        assert_eq!(evaluate_status(1.2001), "WAIT_OVERHEATED");
    }
}
