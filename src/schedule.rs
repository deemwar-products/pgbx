//! Human schedules ("every 1 hour", "daily at 02:30", "weekly on sunday at 03:00") -> cron, plus next-run math.

use chrono::{DateTime, TimeZone, Utc};

pub const HELP: &str = "use: 'hourly', 'daily', 'weekly', 'every N minutes', 'every N hours', \
'daily at HH:MM', 'weekly on <day> at HH:MM', or a 5-field cron like '0 */6 * * *'";

pub fn to_cron(input: &str) -> Result<String, String> {
    let s = input.trim().to_lowercase();
    let words: Vec<&str> = s.split_whitespace().collect();
    let cron = match words.as_slice() {
        ["hourly"] | ["every", "hour"] => "0 * * * *".to_string(),
        ["daily"] | ["every", "day"] => "0 2 * * *".to_string(),
        ["weekly"] | ["every", "week"] => "0 3 * * 0".to_string(),
        ["every", "minute"] => "* * * * *".to_string(),
        ["every", n, unit] => {
            let n: u32 = n.parse().map_err(|_| format!("'{n}' is not a number; {HELP}"))?;
            match unit.trim_end_matches('s') {
                "minute" | "min" if (1..=59).contains(&n) => if n == 1 { "* * * * *".into() } else { format!("*/{n} * * * *") },
                "hour" | "hr" if (1..=23).contains(&n) => if n == 1 { "0 * * * *".into() } else { format!("0 */{n} * * *") },
                "minute" | "min" => return Err("minutes must be 1-59".into()),
                "hour" | "hr" => return Err("hours must be 1-23".into()),
                _ => return Err(format!("unknown unit '{unit}'; {HELP}")),
            }
        }
        ["daily", "at", t] | ["every", "day", "at", t] => {
            let (h, m) = hhmm(t)?;
            format!("{m} {h} * * *")
        }
        ["weekly", "on", d, "at", t] | ["every", d, "at", t] => {
            let (h, m) = hhmm(t)?;
            format!("{m} {h} * * {}", weekday(d)?)
        }
        _ if words.len() == 5 => s.clone(),
        _ => return Err(format!("can't read schedule '{input}'; {HELP}")),
    };
    // whatever we produced (or were given) must parse as cron
    croner::Cron::new(&cron).parse().map_err(|e| format!("invalid schedule '{input}' ({cron}): {e}; {HELP}"))?;
    Ok(cron)
}

fn hhmm(t: &str) -> Result<(u32, u32), String> {
    let (h, m) = t.split_once(':').unwrap_or((t, "0"));
    let (h, m): (u32, u32) = (h.parse().map_err(|_| format!("bad time '{t}'"))?, m.parse().map_err(|_| format!("bad time '{t}'"))?);
    if h > 23 || m > 59 { return Err(format!("bad time '{t}', use HH:MM 24h")); }
    Ok((h, m))
}

fn weekday(d: &str) -> Result<u32, String> {
    let days = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
    days.iter().position(|x| d.starts_with(x)).map(|i| i as u32).ok_or(format!("unknown day '{d}'"))
}

/// Next run strictly after `after`.
pub fn next_after(cron: &str, after: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    croner::Cron::new(cron).parse().map_err(|e| e.to_string())?.find_next_occurrence(&after, false).map_err(|e| e.to_string())
}

pub fn next_after_epoch(cron: &str, after_epoch: f64) -> Result<f64, String> {
    let after = Utc.timestamp_opt(after_epoch.floor() as i64, 0).single().ok_or("bad timestamp")?;
    Ok(next_after(cron, after)?.timestamp() as f64)
}

#[cfg(test)]
mod t {
    use super::to_cron;
    #[test]
    fn forms() {
        for (i, o) in [
            ("every 1 hour", "0 * * * *"), ("hourly", "0 * * * *"), ("every 15 minutes", "*/15 * * * *"),
            ("every 6 hours", "0 */6 * * *"), ("daily at 02:30", "30 2 * * *"), ("Daily", "0 2 * * *"),
            ("weekly on sunday at 03:00", "0 3 * * 0"), ("every monday at 9:15", "15 9 * * 1"), ("0 */6 * * *", "0 */6 * * *"),
        ] {
            assert_eq!(to_cron(i).unwrap(), o, "{i}");
        }
        for bad in ["every 0 hours", "every 90 minutes", "daily at 25:00", "sometimes", "* * *"] {
            assert!(to_cron(bad).is_err(), "{bad}");
        }
    }
}
