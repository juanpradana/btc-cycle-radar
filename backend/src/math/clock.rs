//! Bitcoin Clock & Cycle Timing (1064 / 364 hari, US Midterm).
//!
//! Referensi anchor cycle:
//! - Last Cycle Bottom  : 2022-11-21
//! - Last Cycle ATH     : 2025-10-06 (ATH ~$108k di periode ini)
//!
//! Estimasi historis:
//! - Cycle Minima (bottom berikutnya)  ≈ 350–377 hari **pasca-ATH**.
//! - Cycle Maxima (top berikutnya)     ≈ ~1.064 hari **pasca-bottom**.
//! - US Midterm Election years         = tahun yang habis dibagi 4 (2022, 2026, 2030...).
use chrono::{Datelike, NaiveDate};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CycleAnchors {
    pub last_ath: NaiveDate,
    pub last_bottom: NaiveDate,
}

/// Anchor default berdasarkan data historis siklus 2022–2025.
pub const DEFAULT_ANCHORS: CycleAnchors = CycleAnchors {
    last_ath: NaiveDate::from_ymd_opt(2025, 10, 6).expect("valid"),
    last_bottom: NaiveDate::from_ymd_opt(2022, 11, 21).expect("valid"),
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CycleClock {
    /// Hari sejak ATH siklus terakhir.
    pub days_since_last_ath: i64,
    /// Hari sejak bottom siklus terakhir.
    pub days_since_last_bottom: i64,
    /// Estimasi hari tersisa hingga bottom berikutnya (post-ATH ~350–377d).
    pub days_until_est_cycle_min: i64,
    /// Estimasi hari tersisa hingga top berikutnya (post-bottom ~1064d).
    pub days_until_est_cycle_max: i64,
    /// Apakah tahun ini US Midterm Election year.
    pub is_us_midterm_year: bool,
    /// Fase siklus kasar.
    pub phase: &'static str,
}

/// Analisis cycle clock menggunakan anchor default.
pub fn analyze_cycle(date: NaiveDate) -> CycleClock {
    analyze_cycle_with(date, DEFAULT_ANCHORS)
}

/// Analisis cycle clock dengan anchor custom.
pub fn analyze_cycle_with(date: NaiveDate, anchors: CycleAnchors) -> CycleClock {
    let days_since_ath = (date - anchors.last_ath).num_days();
    let days_since_bottom = (date - anchors.last_bottom).num_days();

    // Estimasi 350–377 hari pasca ATH → bottom berikutnya
    // Pakai midpoint 363 hari. Jika sudah lewat, negatif.
    const EST_CYCLE_MIN_POST_ATH: i64 = 363;
    // Estimasi 1064 hari pasca bottom → top berikutnya
    const EST_CYCLE_MAX_POST_BOTTOM: i64 = 1064;

    let days_until_min = EST_CYCLE_MIN_POST_ATH - days_since_ath;
    let days_until_max = EST_CYCLE_MAX_POST_BOTTOM - days_since_bottom;

    let is_midterm = is_us_midterm_year(date);

    // Tentukan fase kasar
    let phase = if days_since_bottom < 200 {
        "EARLY_ACCUMULATION"
    } else if days_since_bottom < 500 {
        "MID_ACCUMULATION"
    } else if days_since_ath < 0 {
        "PRE_ATH_EXPANSION"
    } else if days_since_ath < 200 {
        "POST_ATH_BULL_RUN"
    } else if days_since_ath < EST_CYCLE_MIN_POST_ATH {
        "LATE_BULL_TOPPING"
    } else {
        "BEAR_MARKET"
    };

    CycleClock {
        days_since_last_ath: days_since_ath,
        days_since_last_bottom: days_since_bottom,
        days_until_est_cycle_min: days_until_min,
        days_until_est_cycle_max: days_until_max,
        is_us_midterm_year: is_midterm,
        phase,
    }
}

/// Tahun US Midterm Election: tahun yang habis dibagi 4 (2022, 2026, 2030...).
/// Tahun 2022 adalah midterm (Nov 2022 election). 2026 juga.
fn is_us_midterm_year(date: NaiveDate) -> bool {
    let year = date.year();
    // Tahun midterm = 2 mod 4 (2022 % 4 = 2, 2026 % 4 = 2)
    year % 4 == 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midterm_year_detection() {
        assert!(is_us_midterm_year(NaiveDate::from_ymd_opt(2022, 11, 8).unwrap()));
        assert!(is_us_midterm_year(NaiveDate::from_ymd_opt(2026, 11, 3).unwrap()));
        assert!(!is_us_midterm_year(NaiveDate::from_ymd_opt(2024, 11, 5).unwrap()));
        assert!(!is_us_midterm_year(NaiveDate::from_ymd_opt(2025, 5, 1).unwrap()));
    }

    #[test]
    fn phase_transitions() {
        // 2023-06-01 (~200 hari post bottom 2022-11-21)
        let c = analyze_cycle(NaiveDate::from_ymd_opt(2023, 6, 1).unwrap());
        assert_eq!(c.phase, "EARLY_ACCUMULATION");

        // 2024-01-01 (~400 hari post bottom)
        let c = analyze_cycle(NaiveDate::from_ymd_opt(2024, 1, 1).unwrap());
        assert_eq!(c.phase, "MID_ACCUMULATION");

        // 2025-10-01 (sebelum ATH)
        let c = analyze_cycle(NaiveDate::from_ymd_opt(2025, 10, 1).unwrap());
        assert_eq!(c.phase, "PRE_ATH_EXPANSION");

        // 2025-11-01 (~25 hari pasca ATH)
        let c = analyze_cycle(NaiveDate::from_ymd_opt(2025, 11, 1).unwrap());
        assert_eq!(c.phase, "POST_ATH_BULL_RUN");

        // 2026-05-01 (~210 hari pasca ATH)
        let c = analyze_cycle(NaiveDate::from_ymd_opt(2026, 5, 1).unwrap());
        assert_eq!(c.phase, "LATE_BULL_TOPPING");

        // 2026-10-15 (> 363 hari pasca ATH 2025-10-06, tepatnya 374 hari)
        let c = analyze_cycle(NaiveDate::from_ymd_opt(2026, 10, 15).unwrap());
        assert_eq!(c.phase, "BEAR_MARKET");
    }
}