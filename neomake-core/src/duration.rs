//! Durations as configurations write them: `"30s"`, `"10m"`, `"1h30m"`,
//! `"500ms"`, or a number of seconds.

use std::time::Duration;

/// Parse a duration such as `"1h30m"`. Units are `ms`, `s`, `m` and `h`;
/// several may be combined, largest first. A bare number is seconds.
pub fn parse(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("a duration cannot be empty".into());
    }
    if let Ok(secs) = text.parse::<f64>() {
        return from_secs(secs, text);
    }
    let mut total = Duration::ZERO;
    let mut rest = text;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .ok_or_else(|| {
                format!("`{text}` needs a unit after its last number (ms, s, m or h)")
            })?;
        if digits == 0 {
            return Err(format!(
                "`{text}` is not a duration such as 30s, 10m or 1h30m"
            ));
        }
        let number: f64 = rest[..digits]
            .parse()
            .map_err(|_| format!("`{text}` is not a duration such as 30s, 10m or 1h30m"))?;
        rest = &rest[digits..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let factor = match &rest[..unit_len] {
            "ms" => 0.001,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            other => {
                return Err(format!(
                    "`{text}` has an unknown unit `{other}` (ms, s, m or h)"
                ))
            }
        };
        rest = &rest[unit_len..];
        total += from_secs(number * factor, text)?;
    }
    Ok(total)
}

fn from_secs(secs: f64, text: &str) -> Result<Duration, String> {
    if !secs.is_finite() || secs < 0.0 {
        return Err(format!("`{text}` is not a positive duration"));
    }
    Ok(Duration::from_secs_f64(secs))
}

/// A duration the way a person reads it: `850ms`, `12.3s`, `4m05s`, `1h02m`.
pub fn human(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", d.as_secs_f64())
    } else if ms < 3_600_000 {
        let s = d.as_secs();
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        let m = d.as_secs() / 60;
        format!("{}h{:02}m", m / 60, m % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units_and_combinations() {
        assert_eq!(parse("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse("1.5s").unwrap(), Duration::from_millis(1500));
        assert_eq!(parse("90").unwrap(), Duration::from_secs(90));
        assert!(parse("10").is_ok());
        assert!(parse("10x").unwrap_err().contains("unknown unit"));
        assert!(parse("m").is_err());
        assert!(parse("5m3").unwrap_err().contains("needs a unit"));
        assert!(parse("").is_err());
    }

    #[test]
    fn formats_for_people() {
        assert_eq!(human(Duration::from_millis(850)), "850ms");
        assert_eq!(human(Duration::from_millis(12_340)), "12.3s");
        assert_eq!(human(Duration::from_secs(245)), "4m05s");
        assert_eq!(human(Duration::from_secs(3720)), "1h02m");
    }
}
