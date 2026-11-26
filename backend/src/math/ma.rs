//! Moving averages & "magic lines".
//!
//! Konvensi: **1 minggu = 7 hari kalender**. Karena data harian BTC kontinu,
//! SMA 50 minggu ≡ SMA 350 hari, 200 minggu ≡ 1.400 hari, 350 minggu ≡ 2.450 hari.
//! Ini adalah definisi yang dipakai praktisi (LookIntoBitcoin / Cowen) dan
//! konsisten dengan perhitungan berbasis candle harian.

/// 50W MA (~350D MA) — gerbang utama bull/bear.
pub const FIFTY_WEEK_DAYS: usize = 50 * 7;
/// 30W MA (~210D MA) — konfirmasi institusi / recovery.
pub const THIRTY_WEEK_DAYS: usize = 30 * 7;
/// 200W MA (~1.400D MA) — magnet bear market.
pub const TWO_HUNDRED_WEEK_DAYS: usize = 200 * 7;
/// 350W MA (~2.450D MA) — batas bawah ekstrem.
pub const THREE_HUNDRED_FIFTY_WEEK_DAYS: usize = 350 * 7;
/// 10W MA (~70D) — momentum jangka menengah (fast leg).
pub const TEN_WEEK_DAYS: usize = 10 * 7;
/// 20W MA (~140D) — momentum jangka menengah (slow leg).
pub const TWENTY_WEEK_DAYS: usize = 20 * 7;
/// 2Y SMA (730D) — "Investor Tool".
pub const TWO_YEAR_DAYS: usize = 730;
/// Pengali 2Y SMA untuk zona overheated (2Y SMA x 5).
pub const INVESTOR_TOOL_TOP_MULTIPLIER: f64 = 5.0;
/// Threshold buyzone 200W MA (harga <= 105% dari 200W MA).
pub const TWO_HUNDRED_WEEK_BUYZONE_FACTOR: f64 = 1.05;

/// SMA sederhana dari `period` nilai **terakhir** pada slice.
///
/// Mengembalikan `None` bila data kurang dari `period` atau ada nilai non-finite.
pub fn simple_moving_average(values: &[f64], period: usize) -> Option<f64> {
    if period == 0 || values.len() < period {
        return None;
    }
    let window = &values[values.len() - period..];
    let mut sum = 0.0;
    for v in window {
        if !v.is_finite() {
            return None;
        }
        sum += v;
    }
    Some(sum / period as f64)
}

/// Seri rolling SMA sepanjang input; elemen `None` bila jendela belum penuh.
/// Panjang keluaran == panjang masukan (selaras dengan tanggal untuk charting).
pub fn rolling_sma(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let mut out = Vec::with_capacity(values.len());
    if period == 0 {
        return vec![None; values.len()];
    }
    let mut sum = 0.0;
    for (i, v) in values.iter().enumerate() {
        if !v.is_finite() {
            // reset window agar nilai rusak tidak meracuni perhitungan berikutnya
            sum = 0.0;
            out.push(None);
            // isi ulang dari jendela terakhir yang valid
            let start = (i + 1).saturating_sub(period - 1);
            for prev in &values[start..i] {
                if prev.is_finite() {
                    sum += prev;
                }
            }
            continue;
        }
        sum += v;
        if i >= period {
            let drop = values[i - period];
            if drop.is_finite() {
                sum -= drop;
            }
        }
        if i + 1 >= period {
            out.push(Some(sum / period as f64));
        } else {
            out.push(None);
        }
    }
    out
}

/// Klasifikasi cross 10W vs 20W MA.
pub fn evaluate_ma_cross(fast: f64, slow: f64) -> &'static str {
    if !fast.is_finite() || !slow.is_finite() || fast <= 0.0 || slow <= 0.0 {
        return "UNKNOWN";
    }
    // toleransi relatif 0.1% agar noise kecil tidak menghasilkan sinyal
    let diff_pct = (fast - slow) / slow * 100.0;
    if diff_pct > 0.1 {
        "GOLDEN_CROSS"
    } else if diff_pct < -0.1 {
        "DEATH_CROSS"
    } else {
        "NEUTRAL"
    }
}

/// Status 50W MA sebagai *Trend Gate* (regime classifier).
///
/// - harga > 50W MA        → `BULL_TREND`
/// - harga <= 50W MA       → `RISK_OFF`
pub fn evaluate_trend_gate_50w(price: f64, sma_50w: f64) -> &'static str {
    if !price.is_finite() || !sma_50w.is_finite() || sma_50w <= 0.0 {
        return "UNKNOWN";
    }
    if price > sma_50w {
        "BULL_TREND"
    } else {
        "RISK_OFF"
    }
}

/// Zona 200W MA: `DEEP_BOTTOM` (< MA), `BUYZONE` (<= 105% MA), selain itu `NEUTRAL`.
pub fn evaluate_200w_zone(price: f64, sma_200w: f64) -> &'static str {
    if !price.is_finite() || !sma_200w.is_finite() || sma_200w <= 0.0 {
        return "UNKNOWN";
    }
    if price < sma_200w {
        "DEEP_BOTTOM"
    } else if price <= sma_200w * TWO_HUNDRED_WEEK_BUYZONE_FACTOR {
        "BUYZONE"
    } else {
        "NEUTRAL"
    }
}

/// Investor Tool (2Y SMA & 2Y SMA x 5).
pub fn evaluate_investor_tool(price: f64, sma_2y: f64) -> &'static str {
    if !price.is_finite() || !sma_2y.is_finite() || sma_2y <= 0.0 {
        return "UNKNOWN";
    }
    if price > sma_2y * INVESTOR_TOOL_TOP_MULTIPLIER {
        "OVERHEATED"
    } else if price < sma_2y {
        "BUYZONE"
    } else {
        "NEUTRAL"
    }
}

/// Jarak persentase harga ke sebuah level MA (positif = level di atas spot).
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
    fn rolling_sma_matches_trailing_window() {
        let v = vec![1.0, 2.0, 3.0, 4.0];
        let s = rolling_sma(&v, 2);
        assert_eq!(s.len(), 4);
        assert_eq!(s[0], None);
        assert_eq!(s[1], Some(1.5));
        assert_eq!(s[2], Some(2.5));
        assert_eq!(s[3], Some(3.5));
    }

    #[test]
    fn week_constants_are_seven_day_multiples() {
        assert_eq!(FIFTY_WEEK_DAYS, 350);
        assert_eq!(THIRTY_WEEK_DAYS, 210);
        assert_eq!(TWO_HUNDRED_WEEK_DAYS, 1400);
        assert_eq!(THREE_HUNDRED_FIFTY_WEEK_DAYS, 2450);
        assert_eq!(TEN_WEEK_DAYS, 70);
        assert_eq!(TWENTY_WEEK_DAYS, 140);
    }

    #[test]
    fn invalid_input_never_panics() {
        assert_eq!(evaluate_ma_cross(f64::NAN, 1.0), "UNKNOWN");
        assert_eq!(evaluate_200w_zone(100.0, 0.0), "UNKNOWN");
        assert_eq!(distance_pct(0.0, 10.0), None);
    }
}
