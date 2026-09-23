use std::fmt;
use std::time::Duration;

use anyhow::{bail, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A duration written as `45m`, `2h`, `1h30m`, `90s`, `500ms` or a bare
/// number of seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HumanDuration(pub Duration);

impl HumanDuration {
    pub const fn from_secs(s: u64) -> Self {
        Self(Duration::from_secs(s))
    }
    pub fn as_secs(&self) -> u64 {
        self.0.as_secs()
    }
    pub fn as_duration(&self) -> Duration {
        self.0
    }

    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.is_empty() {
            bail!("empty duration");
        }
        if let Ok(n) = s.parse::<u64>() {
            return Ok(Self::from_secs(n));
        }
        let mut total = Duration::ZERO;
        let mut num = String::new();
        let mut chars = s.chars().peekable();
        let mut any = false;
        while let Some(c) = chars.next() {
            if c.is_ascii_digit() {
                num.push(c);
                continue;
            }
            let mut unit = c.to_string();
            if c == 'm' && chars.peek() == Some(&'s') {
                unit.push(chars.next().unwrap());
            }
            let n: u64 = num.parse().map_err(|_| anyhow::anyhow!("invalid duration {s:?}"))?;
            num.clear();
            total += match unit.as_str() {
                "ms" => Duration::from_millis(n),
                "s" => Duration::from_secs(n),
                "m" => Duration::from_secs(n * 60),
                "h" => Duration::from_secs(n * 3600),
                "d" => Duration::from_secs(n * 86400),
                _ => bail!("invalid duration unit {unit:?} in {s:?}"),
            };
            any = true;
        }
        if !num.is_empty() || !any {
            bail!("invalid duration {s:?} (use e.g. 45m, 2h, 1h30m, 90s)");
        }
        Ok(Self(total))
    }
}

impl fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.0.as_secs();
        if self.0.subsec_millis() != 0 && s == 0 {
            return write!(f, "{}ms", self.0.as_millis());
        }
        let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
        let mut out = String::new();
        if h > 0 {
            out.push_str(&format!("{h}h"));
        }
        if m > 0 {
            out.push_str(&format!("{m}m"));
        }
        if sec > 0 || out.is_empty() {
            out.push_str(&format!("{sec}s"));
        }
        f.write_str(&out)
    }
}

impl Serialize for HumanDuration {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            N(u64),
            S(String),
        }
        match Raw::deserialize(d)? {
            Raw::N(n) => Ok(Self::from_secs(n)),
            Raw::S(s) => Self::parse(&s).map_err(serde::de::Error::custom),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_display() {
        assert_eq!(HumanDuration::parse("45m").unwrap().as_secs(), 2700);
        assert_eq!(HumanDuration::parse("1h30m").unwrap().as_secs(), 5400);
        assert_eq!(HumanDuration::parse("90").unwrap().as_secs(), 90);
        assert_eq!(HumanDuration::parse("500ms").unwrap().0, Duration::from_millis(500));
        assert!(HumanDuration::parse("5x").is_err());
        assert!(HumanDuration::parse("m").is_err());
        assert!(HumanDuration::parse("5m3").is_err());
        assert_eq!(HumanDuration::from_secs(5400).to_string(), "1h30m");
        assert_eq!(HumanDuration::from_secs(0).to_string(), "0s");
    }
}
