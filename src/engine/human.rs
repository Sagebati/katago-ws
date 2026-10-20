//! Human SL profiles: whose play KataGo's human network imitates.
//!
//! KataGo fails a whole query over a profile it doesn't know (`Unknown human
//! SL network profile`), which for a queued job means every retry fails the
//! same way. So a profile is checked when the job is submitted, against the
//! grammar KataGo v1.16 accepts (`SGFMetadata::getProfile`):
//!
//! - `rank_{R}` / `preaz_{R}` — one rank, `20k` … `1k`, `1d` … `9d`;
//! - `rank_{B}_{W}` / `preaz_{B}_{W}` — Black's and White's ranks;
//! - `proyear_{Y}` — pro games around year `1800` … `2023`.

use crate::error::{AppError, AppResult};

/// Whether `rank` is one of KataGo's rank names (`20k` … `1k`, `1d` … `9d`).
fn is_rank(rank: &str) -> bool {
    let (number, unit) = rank.split_at(rank.len().saturating_sub(1));
    // No leading zeros or signs: KataGo matches the exact strings.
    if number.starts_with('0') || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    match (number.parse::<u8>(), unit) {
        (Ok(kyu), "k") => (1..=20).contains(&kyu),
        (Ok(dan), "d") => (1..=9).contains(&dan),
        _ => false,
    }
}

/// Check a human SL profile name, as KataGo would.
pub fn validate(profile: &str) -> AppResult<()> {
    let valid = if let Some(year) = profile.strip_prefix("proyear_") {
        year.len() == 4
            && year.bytes().all(|byte| byte.is_ascii_digit())
            && year
                .parse::<u16>()
                .is_ok_and(|year| (1800..=2023).contains(&year))
    } else if let Some(ranks) = profile
        .strip_prefix("rank_")
        .or_else(|| profile.strip_prefix("preaz_"))
    {
        match ranks.split_once('_') {
            Some((black, white)) => is_rank(black) && is_rank(white),
            None => is_rank(ranks),
        }
    } else {
        false
    };
    if valid {
        Ok(())
    } else {
        Err(AppError::BadRequest(format!(
            "unknown human_profile '{profile}': expected rank_<R>, rank_<B>_<W>, \
             preaz_<R>, preaz_<B>_<W> (R from 20k to 9d) or proyear_<1800..2023>"
        )))
    }
}

/// A submitted profile, normalised: an absent or blank one means none.
pub fn parse(profile: Option<String>) -> AppResult<Option<String>> {
    match profile.map(|profile| profile.trim().to_owned()) {
        Some(profile) if !profile.is_empty() => {
            validate(&profile)?;
            Ok(Some(profile))
        }
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_every_profile_family_katago_does() {
        for profile in [
            "rank_20k",
            "rank_5k",
            "rank_1k",
            "rank_1d",
            "rank_9d",
            "preaz_3d",
            "rank_1d_5k",
            "preaz_20k_9d",
            "proyear_1800",
            "proyear_2023",
        ] {
            assert!(validate(profile).is_ok(), "{profile} should be accepted");
        }
    }

    /// Each of these makes KataGo fail the query (checked against v1.16.5).
    #[test]
    fn refuses_what_katago_would_fail_the_query_over() {
        for profile in [
            "",
            "rank_99k",
            "rank_21k",
            "rank_0k",
            "rank_10d",
            "rank_05k",
            "rank_5",
            "rank_5k_",
            "rank_5k_1d_2d",
            "proyear_1799",
            "proyear_2024",
            "proyear_+999",
            "RANK_5k",
            "5k",
        ] {
            assert!(validate(profile).is_err(), "{profile:?} should be refused");
        }
    }

    #[test]
    fn a_blank_profile_is_no_profile() {
        assert_eq!(parse(None).unwrap(), None);
        assert_eq!(parse(Some("  ".to_owned())).unwrap(), None);
        assert_eq!(
            parse(Some(" rank_5k ".to_owned())).unwrap().as_deref(),
            Some("rank_5k")
        );
        assert!(parse(Some("rank_99k".to_owned())).is_err());
    }
}
