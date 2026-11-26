//! 200W MA Quantile Regression — persentil historis rasio Price / 200W MA.
//!
//! Logika:
//! - Ambil seluruh riwayat harian `ratio = close / sma_200w` (hanya di mana SMA tersedia).
//! - Persentil `p` = (jumlah nilai <= `ratio` saat ini / total) * 100.
//! - `< 10`   → `BUYZONE`
//! - `> 90`   → `TOP_ZONE`
//! - selain itu → `NEUTRAL`

/// Hitung persentil (0–100) dari `value` dalam `history` (ascending).
///
/// Mengembalikan `None` bila history kosong atau `value` non-finite.
pub fn calculate_percentile(value: f64, history: &[f64]) -> Option<f64> {
    if history.is_empty() || !value.is_finite() {
        return None;
    }
    let count_le = history.iter().filter(|&&x| x.is_finite() && x <= value).count();
    let total = history.iter().filter(|x| x.is_finite()).count();
    if total == 0 {
        return None;
    }
    Some((count_le as f64 / total as f64) * 100.0)
}

/// Status kuantil 200W MA.
pub fn evaluate_quantile_status(percentile: f64) -> &'static str {
    if !percentile.is_finite() {
        return "UNKNOWN";
    }
    if percentile < 10.0 {
        "BUYZONE"
    } else if percentile > 90.0 {
        "TOP_ZONE"
    } else {
        "NEUTRAL"
    }
}

/// Menghasilkan seri rasio `close / sma_200w` dari data harian.
///
/// Mengembalikan `None` di posisi di mana SMA belum tersedia.
pub fn ratio_series(closes: &[f64], sma_200w: &[Option<f64>]) -> Vec<Option<f64>> {
    assert_eq!(closes.len(), sma_200w.len());
    closes
        .iter()
        .zip(sma_200w.iter())
        .map(|(c, m)| {
            if let Some(m) = m {
                if *m > 0.0 && c.is_finite() && m.is_finite() {
                    Some(c / m)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect()
}

/// Rolling percentile untuk setiap titik data (menggunakan history sampai titik tsb).
///
/// `window` = ukuran jendela lookback (mis. 365*4 = 4 tahun). Mengembalikan `None`
/// bila data lookback belum cukup.
pub fn rolling_percentile(values: &[f64], window: usize) -> Vec<Option<f64>> {
    let mut out = Vec::with_capacity(values.len());
    for i in 0..values.len() {
        if i == 0 {
            out.push(None);
            continue;
        }
        let start = i.saturating_sub(window);
        let hist = &values[start..i];
        let p = calculate_percentile(values[i], hist);
        out.push(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_linear_1_to_10() {
        let hist: Vec<f64> = (1..=10).map(|x| x as f64).collect();
        assert_eq!(calculate_percentile(1.0, &hist), Some(10.0));
        assert_eq!(calculate_percentile(10.0, &hist), Some(100.0));
        let p5 = calculate_percentile(5.5, &hist).unwrap();
        assert!((p5 - 50.0).abs() <= 5.0);
    }

    #[test]
    fn status_boundaries() {
        assert_eq!(evaluate_quantile_status(9.9), "BUYZONE");
        assert_eq!(evaluate_quantile_status(10.0), "NEUTRAL");
        assert_eq!(evaluate_quantile_status(90.0), "NEUTRAL");
        assert_eq!(evaluate_quantile_status(90.1), "TOP_ZONE");
    }
}