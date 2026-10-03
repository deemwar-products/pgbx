//! Which backups to delete. Pure (no pgrx, no S3) so the selection is unit-tested.
//!
//! Rules, combined:
//!   * simple: keep at most `max_backups` newest AND nothing older than `max_days`;
//!   * GFS (optional, e.g. '7d,4w,12m'): additionally keep the newest backup of each of the last 7 calendar
//!     days, last 4 ISO weeks and last 12 calendar months (UTC; 'y' = years). A GFS keeper is never deleted
//!     by the simple rule;
//!   * the newest backup is always kept.

use chrono::{DateTime, Datelike, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Gfs {
    pub days: u32,
    pub weeks: u32,
    pub months: u32,
    pub years: u32,
}

impl Gfs {
    /// '7d,4w,12m' (any order, any subset; 'y' for years). '' / 'off' / 'none' -> None.
    pub fn parse(spec: &str) -> Result<Option<Gfs>, String> {
        let s = spec.trim().to_ascii_lowercase();
        let s = s.strip_prefix("gfs:").unwrap_or(&s);
        if s.is_empty() || s == "off" || s == "none" {
            return Ok(None);
        }
        let mut g = Gfs::default();
        for part in s.split(',').map(str::trim) {
            let (num, unit) = part.split_at(part.len().saturating_sub(1));
            let n: u32 = num.trim().parse().map_err(|_| format!("gfs '{spec}': '{part}' should look like 7d, 4w, 12m or 2y"))?;
            if n == 0 || n > 1000 {
                return Err(format!("gfs '{spec}': '{part}' must be between 1 and 1000"));
            }
            let slot = match unit {
                "d" => &mut g.days,
                "w" => &mut g.weeks,
                "m" => &mut g.months,
                "y" => &mut g.years,
                _ => return Err(format!("gfs '{spec}': '{part}' should look like 7d, 4w, 12m or 2y")),
            };
            if *slot != 0 {
                return Err(format!("gfs '{spec}': '{unit}' given twice"));
            }
            *slot = n;
        }
        Ok(Some(g))
    }

    /// How far back (days) the oldest GFS keeper can reach; checked against pgbx.max_days_limit.
    pub fn span_days(&self) -> i64 {
        [self.days as i64, self.weeks as i64 * 7, self.months as i64 * 31, self.years as i64 * 366].into_iter().max().unwrap_or(0)
    }
}

fn buckets(t: DateTime<Utc>) -> [i64; 4] {
    let d = t.date_naive();
    let day = d.num_days_from_ce() as i64;
    let week = (day - d.weekday().num_days_from_monday() as i64) / 7; // Monday-based week number
    [day, week, d.year() as i64 * 12 + d.month0() as i64, d.year() as i64]
}

/// For each backup time (any order), whether GFS keeps it.
pub fn gfs_keep(times: &[DateTime<Utc>], g: &Gfs, now: DateTime<Utc>) -> Vec<bool> {
    let mut keep = vec![false; times.len()];
    let nb = buckets(now);
    for (k, n) in [g.days, g.weeks, g.months, g.years].into_iter().enumerate() {
        if n == 0 {
            continue;
        }
        // newest backup per bucket, for buckets within the last n (current one included)
        let mut best: std::collections::HashMap<i64, usize> = Default::default();
        for (i, t) in times.iter().enumerate() {
            let b = buckets(*t)[k];
            if b > nb[k] || nb[k] - b >= n as i64 {
                continue;
            }
            match best.get(&b) {
                Some(&j) if times[j] >= *t => {}
                _ => {
                    best.insert(b, i);
                }
            }
        }
        for i in best.into_values() {
            keep[i] = true;
        }
    }
    keep
}

/// Indices (into `times`, sorted oldest first) to delete. Unparseable times (None) are only deleted by count.
pub fn to_delete(times: &[Option<DateTime<Utc>>], max_backups: i32, max_days: i32, gfs: Option<&Gfs>, now: DateTime<Utc>) -> Vec<usize> {
    let cutoff = now - chrono::Duration::days(max_days.max(1) as i64);
    let keep_from = times.len().saturating_sub(max_backups.max(1) as usize);
    let g = gfs.map(|g| {
        let known: Vec<(usize, DateTime<Utc>)> = times.iter().enumerate().filter_map(|(i, t)| t.map(|t| (i, t))).collect();
        let flags = gfs_keep(&known.iter().map(|x| x.1).collect::<Vec<_>>(), g, now);
        let mut v = vec![false; times.len()];
        for ((i, _), f) in known.iter().zip(flags) {
            v[*i] = f;
        }
        v
    });
    (0..times.len().saturating_sub(1)) // newest: always kept
        .filter(|&i| {
            let simple_keep = i >= keep_from && !times[i].is_some_and(|t| t < cutoff);
            let gfs_keep = g.as_ref().is_some_and(|v| v[i]);
            !simple_keep && !gfs_keep
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    #[test]
    fn parses_specs() {
        assert_eq!(Gfs::parse("7d,4w,12m").unwrap(), Some(Gfs { days: 7, weeks: 4, months: 12, years: 0 }));
        assert_eq!(Gfs::parse("gfs:2y, 30d").unwrap(), Some(Gfs { days: 30, weeks: 0, months: 0, years: 2 }));
        assert_eq!(Gfs::parse("off").unwrap(), None);
        assert_eq!(Gfs::parse("").unwrap(), None);
        for bad in ["7x", "d", "0d", "7d,8d", "seven days"] {
            assert!(Gfs::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(Gfs::parse("7d,4w,12m").unwrap().unwrap().span_days(), 372);
    }

    #[test]
    fn gfs_keeps_newest_per_bucket() {
        // hourly backups for 400 days, ending now
        let now = at(2026, 10, 1, 12);
        let times: Vec<DateTime<Utc>> = (0..400 * 24).rev().map(|h| now - chrono::Duration::hours(h)).collect();
        let g = Gfs::parse("7d,4w,12m").unwrap().unwrap();
        let keep = gfs_keep(&times, &g, now);
        let kept: Vec<DateTime<Utc>> = times.iter().zip(&keep).filter(|x| *x.1).map(|x| *x.0).collect();
        // 7 daily (today's newest = now, then 23:00 of the 6 days before)
        for d in 1..7 {
            let day_end = at(2026, 10, 1, 23) - chrono::Duration::days(d);
            assert!(kept.contains(&day_end), "daily keeper {day_end}");
        }
        // 12 monthly: last backup of each previous month, e.g. 2026-08-31 23:00, 2025-11-30 23:00
        assert!(kept.contains(&at(2026, 8, 31, 23)));
        assert!(kept.contains(&at(2025, 11, 30, 23)));
        assert!(!kept.contains(&at(2025, 10, 31, 23)), "13th month back is not kept");
        // daily (7) + weekly (4, overlapping) + monthly (12, overlapping) => small set
        assert!(kept.len() <= 7 + 4 + 12 && kept.len() >= 12, "{}", kept.len());
    }

    #[test]
    fn prune_selection_combines_rules() {
        let now = at(2026, 10, 1, 12);
        let times: Vec<Option<DateTime<Utc>>> = (0..120).rev().map(|d| Some(now - chrono::Duration::days(d))).collect();
        // simple only: keep newest 14 within 90 days
        let del = to_delete(&times, 14, 90, None, now);
        assert_eq!(times.len() - del.len(), 14);
        assert!(!del.contains(&(times.len() - 1)));
        // with GFS 7d,4w,3m: more survive, and the newest of each of the last 3 months is among them
        let g = Gfs::parse("7d,4w,3m").unwrap().unwrap();
        let del = to_delete(&times, 3, 90, Some(&g), now);
        let kept: Vec<DateTime<Utc>> = times.iter().enumerate().filter(|(i, _)| !del.contains(i)).map(|(_, t)| t.unwrap()).collect();
        assert!(kept.contains(&at(2026, 8, 31, 12)) && kept.contains(&at(2026, 9, 30, 12)));
        assert!(kept.contains(&now));
        assert!(kept.len() >= 7 && kept.len() <= 7 + 4 + 3);
        // GFS keepers survive max_days
        let del = to_delete(&times, 1, 1, Some(&g), now);
        let kept: Vec<_> = times.iter().enumerate().filter(|(i, _)| !del.contains(i)).map(|(_, t)| t.unwrap()).collect();
        assert!(kept.contains(&at(2026, 8, 31, 12)));
        // the newest is kept even with nothing else
        assert_eq!(to_delete(&[Some(now - chrono::Duration::days(500))], 1, 1, None, now), Vec::<usize>::new());
        // unparseable names fall back to count only
        let mixed = vec![None, None, Some(now)];
        assert_eq!(to_delete(&mixed, 2, 1, None, now), vec![0]);
    }
}
