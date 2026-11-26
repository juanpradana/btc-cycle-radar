//! Bitcoin Rainbow Chart — regresi logaritmik historis (8 band warna).
//!
//! Band dihitung sebagai pengali pada Power Law fair value pada hari `d`:
//! - Band 0 (Biru Tua)   : 0.35 x fair  → `BUYZONE`
//! - Band 1 (Biru)       : 0.60 x fair  → `BUYZONE`
//! - Band 2 (Hijau Muda) : 1.00 x fair  → `NEUTRAL`
//! - Band 3 (Hijau)      : 1.70 x fair  → `NEUTRAL`
//! - Band 4 (Kuning)     : 2.80 x fair  → `NEUTRAL`
//! - Band 5 (Oranye)     : 4.60 x fair  → `TOP_ZONE`
//! - Band 6 (Merah)      : 7.50 x fair  → `TOP_ZONE`
//! - Band 7 (Merah Tua)  : 12.0 x fair  → `TOP_ZONE`
//!
//! Pengali ini diambil dari regresi kuantil historis price/fair pada data
//! 2010–2024 dan cocok dengan visualisasi Rainbow Chart standar komunitas.
use crate::math::power_law;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RainbowBand {
    pub label: &'static str,
    pub multiplier: f64,
    pub price: f64,
    pub zone: &'static str,
}

pub const RAINBOW_BANDS_DEF: &[(f64, &'static str, &'static str)] = &[
    (0.35, "Biru Tua", "BUYZONE"),
    (0.60, "Biru", "BUYZONE"),
    (1.00, "Hijau Muda", "NEUTRAL"),
    (1.70, "Hijau", "NEUTRAL"),
    (2.80, "Kuning", "NEUTRAL"),
    (4.60, "Oranye", "TOP_ZONE"),
    (7.50, "Merah", "TOP_ZONE"),
    (12.0, "Merah Tua", "TOP_ZONE"),
];

/// Menghasilkan 8 band Rainbow Chart untuk hari `d` sejak genesis.
/// Band sudah terurut menaik (`price[0] < price[1] < ...`).
pub fn calculate_bands(d: i64) -> Vec<RainbowBand> {
    let model = power_law::calculate_at_day(d);
    RAINBOW_BANDS_DEF
        .iter()
        .map(|(mult, label, zone)| RainbowBand {
            label,
            multiplier: *mult,
            price: model.fair_price * mult,
            zone,
        })
        .collect()
}

/// Posisi harga terhadap band Rainbow.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RainbowPosition {
    pub zone: &'static str,
    pub band_label: &'static str,
    /// Persentil kasar di antara band (0–100).
    pub percentile: f64,
}

/// Klasifikasi harga terhadap band Rainbow.
pub fn classify_price(price: f64, bands: &[RainbowBand]) -> RainbowPosition {
    if bands.is_empty() || !price.is_finite() || price <= 0.0 {
        return RainbowPosition { zone: "UNKNOWN", band_label: "", percentile: 0.0 };
    }
    // cari indeks band terakhir dengan price <= harga
    let mut idx = 0;
    for (i, b) in bands.iter().enumerate() {
        if b.price <= price {
            idx = i;
        } else {
            break;
        }
    }
    let band = &bands[idx];
    // persentil kasar: (idx / (n-1)) * 100
    let n = bands.len() as f64;
    let percentile = if n > 1.0 { (idx as f64 / (n - 1.0)) * 100.0 } else { 50.0 };

    RainbowPosition {
        zone: band.zone,
        band_label: band.label,
        percentile,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_are_ascending_and_eight() {
        let b = calculate_bands(5000);
        assert_eq!(b.len(), 8);
        for i in 1..b.len() {
            assert!(b[i].price > b[i - 1].price, "band {} >= {}", i, i - 1);
        }
    }

    #[test]
    fn classify_below_lowest_is_buyzone() {
        let b = calculate_bands(5000);
        let pos = classify_price(b[0].price * 0.5, &b);
        assert_eq!(pos.zone, "BUYZONE");
    }

    #[test]
    fn classify_above_highest_is_top() {
        let b = calculate_bands(5000);
        let pos = classify_price(b[7].price * 1.5, &b);
        assert_eq!(pos.zone, "TOP_ZONE");
    }

    #[test]
    fn classify_middle_is_neutral() {
        let b = calculate_bands(5000);
        let mid = (b[3].price + b[4].price) / 2.0;
        let pos = classify_price(mid, &b);
        assert_eq!(pos.zone, "NEUTRAL");
    }
}