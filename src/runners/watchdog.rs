//! Watchdog for agents in panes: notices an agent that is busy but not
//! getting anywhere, and hands it to a human early instead of letting it
//! burn tokens until the step timeout.
//!
//! Signals, observed every `check_every` while the agent works:
//!
//! - **(a) stall**: the pane's visible tail has not changed for `stall_after`;
//! - **(b) repeat**: the same error line keeps coming back — seen in
//!   `repeat_threshold` observations that each followed a change of the
//!   screen, over at least `stall_after / 4`;
//! - **(c) no progress**: the worktree has not changed for `idle_after`,
//!   while the agent used at least `burn_tokens` tokens in that time (or its
//!   usage is unknown).
//!
//! It fires on (a)+(c) or (b)+(c) — never on a stall alone, because long test
//! runs stall the screen too, and never while files are changing. Firing
//! never fails the step: the step waits for a human (`awaiting_human`) with
//! the reason, and the watchdog re-arms once the agent makes progress.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::HumanDuration;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct WatchdogConfig {
    pub enabled: bool,
    pub check_every: HumanDuration,
    pub stall_after: HumanDuration,
    pub idle_after: HumanDuration,
    pub burn_tokens: u64,
    pub repeat_threshold: u32,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            check_every: HumanDuration::from_secs(60),
            stall_after: HumanDuration::from_secs(20 * 60),
            idle_after: HumanDuration::from_secs(15 * 60),
            burn_tokens: 150_000,
            repeat_threshold: 4,
        }
    }
}

/// One observation of a working agent.
#[derive(Debug, Clone)]
pub struct Observation {
    pub at: Instant,
    /// Last lines of the agent's pane.
    pub tail: String,
    /// Anything that changes when the worktree changes.
    pub worktree: String,
    /// Tokens used since the step started, when known.
    pub tokens: Option<u64>,
}

#[derive(Debug)]
pub struct Watchdog {
    cfg: WatchdogConfig,
    tail: Option<(u64, Instant)>,
    worktree: Option<(String, Instant, Option<u64>)>,
    /// normalized error line → (times seen after a screen change, first seen)
    errors: BTreeMap<String, (u32, Instant)>,
    fired: bool,
}

fn hash(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Error-looking lines, normalized so counters and timings do not make the
/// same failure look different.
fn error_lines(tail: &str) -> Vec<String> {
    let mut out: Vec<String> = tail
        .lines()
        .map(str::trim)
        .filter(|l| l.len() >= 12)
        .filter(|l| {
            let x = l.to_ascii_lowercase();
            ["error", "failed", "panicked", "exception", "traceback", "cannot find", "not found", "permission denied"].iter().any(|w| x.contains(w))
        })
        .map(|l| l.chars().map(|c| if c.is_ascii_digit() { '#' } else { c }).collect::<String>())
        .collect();
    out.sort();
    out.dedup();
    out
}

impl Watchdog {
    pub fn new(cfg: WatchdogConfig) -> Self {
        Self { cfg, tail: None, worktree: None, errors: BTreeMap::new(), fired: false }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    pub fn check_every(&self) -> Duration {
        self.cfg.check_every.as_duration().max(Duration::from_millis(200))
    }

    pub fn fired(&self) -> bool {
        self.fired
    }

    /// Feed one observation. Returns `Some(reason)` when the watchdog fires;
    /// it fires once, until the agent makes progress again ([`Watchdog::fired`]).
    pub fn observe(&mut self, o: &Observation) -> Option<String> {
        let now = o.at;
        // Screen.
        let th = hash(o.tail.trim());
        let screen_changed = match self.tail {
            Some((h, _)) if h == th => false,
            _ => {
                self.tail = Some((th, now));
                true
            }
        };
        let stalled_for = self.tail.map(|(_, t)| now.duration_since(t)).unwrap_or_default();
        // Worktree.
        let worktree_changed = match &self.worktree {
            Some((w, _, _)) if *w == o.worktree => false,
            _ => {
                self.worktree = Some((o.worktree.clone(), now, o.tokens));
                true
            }
        };
        if worktree_changed {
            // Progress: forget old errors and re-arm.
            self.errors.clear();
            self.fired = false;
            return None;
        }
        let (_, since, tokens_at) = self.worktree.clone().unwrap();
        let idle_for = now.duration_since(since);
        let burned = match (o.tokens, tokens_at) {
            (Some(t), Some(t0)) => Some(t.saturating_sub(t0)),
            (Some(t), None) => Some(t),
            _ => None,
        };
        // Errors that come back after the screen changed.
        if screen_changed {
            for e in error_lines(&o.tail) {
                let entry = self.errors.entry(e).or_insert((0, now));
                entry.0 += 1;
            }
        }
        if self.fired {
            return None;
        }
        let no_progress = idle_for >= self.cfg.idle_after.as_duration() && burned.is_none_or(|b| b >= self.cfg.burn_tokens);
        if !no_progress {
            return None;
        }
        let tokens_note = match burned {
            Some(b) => format!("{b} tokens used"),
            None => "token usage unknown".into(),
        };
        let idle_min = idle_for.as_secs() / 60;
        if stalled_for >= self.cfg.stall_after.as_duration() {
            self.fired = true;
            return Some(format!("no output change for {} min and no file changes for {idle_min} min ({tokens_note})", stalled_for.as_secs() / 60));
        }
        let min_span = self.cfg.stall_after.as_duration() / 4;
        if let Some((line, (n, _))) = self.errors.iter().find(|(_, (n, first))| *n >= self.cfg.repeat_threshold && now.duration_since(*first) >= min_span) {
            self.fired = true;
            let short: String = line.chars().take(120).collect();
            return Some(format!("the same error keeps coming back ({n}×): \"{short}\"; no file changes for {idle_min} min ({tokens_note})"));
        }
        None
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> WatchdogConfig {
        WatchdogConfig {
            enabled: true,
            check_every: HumanDuration::from_secs(1),
            stall_after: HumanDuration::from_secs(600),
            idle_after: HumanDuration::from_secs(300),
            burn_tokens: 1000,
            repeat_threshold: 3,
        }
    }

    fn obs(t0: Instant, secs: u64, tail: &str, wt: &str, tokens: Option<u64>) -> Observation {
        Observation { at: t0 + Duration::from_secs(secs), tail: tail.into(), worktree: wt.into(), tokens }
    }

    #[test]
    fn stall_alone_does_not_fire_but_stall_with_burn_does() {
        let t0 = Instant::now();
        let mut w = Watchdog::new(cfg());
        assert!(w.observe(&obs(t0, 0, "running tests…", "a", Some(0))).is_none());
        // A long, quiet test run: screen and files unchanged, few tokens.
        assert!(w.observe(&obs(t0, 700, "running tests…", "a", Some(10))).is_none());
        // Tokens keep being burned without file changes.
        let r = w.observe(&obs(t0, 800, "running tests…", "a", Some(5000))).unwrap();
        assert!(r.contains("no output change") && r.contains("5000 tokens"), "{r}");
        // Fires once…
        assert!(w.observe(&obs(t0, 900, "running tests…", "a", Some(9000))).is_none());
        assert!(w.fired());
        // …and re-arms when files change.
        assert!(w.observe(&obs(t0, 950, "editing", "b", Some(9500))).is_none());
        assert!(!w.fired());
    }

    #[test]
    fn repeated_errors_fire_before_a_full_stall() {
        let t0 = Instant::now();
        let mut w = Watchdog::new(cfg());
        let mut fired_at = None;
        for i in 1..=8u64 {
            let tail = format!("attempt {i}\nerror[E0425]: cannot find value `x` in this scope (line {i})\n");
            if let Some(r) = w.observe(&obs(t0, i * 60, &tail, "a", None)) {
                assert!(r.contains("keeps coming back"), "{r}");
                fired_at = Some(i * 60);
                break;
            }
        }
        // Needs 5 min without file changes (from the first sighting at 60 s),
        // but not the 10 min screen stall.
        assert_eq!(fired_at, Some(360));
    }

    #[test]
    fn changing_files_never_fire() {
        let t0 = Instant::now();
        let mut w = Watchdog::new(cfg());
        for i in 0..30 {
            let r = w.observe(&obs(t0, i * 100, "same screen\nerror: same thing failed", &format!("wt{i}"), Some(i * 100_000)));
            assert!(r.is_none());
        }
    }

    #[test]
    fn unknown_tokens_count_as_burning() {
        let t0 = Instant::now();
        let mut w = Watchdog::new(cfg());
        w.observe(&obs(t0, 0, "x", "a", None));
        assert!(w.observe(&obs(t0, 650, "x", "a", None)).unwrap().contains("usage unknown"));
    }
}
