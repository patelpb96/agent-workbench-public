//! Minimal ISO-8601 UTC parser (no external crate): `YYYY-MM-DDTHH:MM:SS[.fff](Z|+00:00)`.

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn parse_iso_ms(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, sec) = (num(0, 4)?, num(5, 7)?, num(8, 10)?, num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut i = 19;
    let mut frac_ms = 0i64;
    if b.get(i) == Some(&b'.') {
        let start = i + 1;
        i = start;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        let digits = &s[start..i];
        if digits.is_empty() {
            return None;
        }
        let take: String = digits.chars().chain(std::iter::repeat('0')).take(3).collect();
        frac_ms = take.parse().ok()?;
    }
    match &s[i..] {
        "Z" | "+00:00" => {}
        _ => return None,
    }
    let secs = days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + sec;
    (secs >= 0).then(|| secs as u64 * 1000 + frac_ms as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_utc_forms_and_rejects_garbage() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_ms("2026-10-21T00:00:00.250Z"), Some(1_792_540_800_250));
        assert_eq!(parse_iso_ms("2026-10-21T00:00:00.5+00:00"), Some(1_792_540_800_500));
        assert_eq!(parse_iso_ms("2026-10-21T00:00:00"), None);
        assert_eq!(parse_iso_ms("2026-13-21T00:00:00Z"), None);
        assert_eq!(parse_iso_ms("not a time"), None);
        assert_eq!(parse_iso_ms("2026-10-21T00:00:00+05:00"), None);
    }
}
