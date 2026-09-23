//! Local-only usage accounting. Nothing here ever leaves the machine:
//! there is no external telemetry, analytics or crash upload.
//!
//! Numbers keep their provenance (`measured | reported | estimated |
//! unknown`) all the way to the UI, so "$1.84 reported" and "~$1.84
//! estimated" are never confused.

use serde::Serialize;

use crate::model::{Run, UsageRecord, UsageSource};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct UsageSummary {
    pub runs: usize,
    pub agent_steps: usize,
    pub agent_steps_with_usage: usize,
    pub total: UsageRecord,
}

/// Aggregate agent usage across runs (command runtimes are excluded).
pub fn summarize<'a>(runs: impl Iterator<Item = &'a Run>) -> UsageSummary {
    let mut n = 0;
    let mut steps = 0;
    let mut with = 0;
    let mut recs = vec![];
    for r in runs {
        n += 1;
        for e in &r.steps {
            if e.kind == crate::model::StepKind::Agent {
                steps += 1;
                if let Some(u) = &e.usage {
                    if u.source != UsageSource::Unknown {
                        with += 1;
                    }
                    recs.push(u.clone());
                }
            }
        }
    }
    UsageSummary { runs: n, agent_steps: steps, agent_steps_with_usage: with, total: UsageRecord::sum(recs.iter()) }
}

/// Human-readable tokens line.
pub fn tokens_display(u: &UsageRecord) -> String {
    match (u.input_tokens, u.output_tokens) {
        (None, None) => "tokens unknown".into(),
        (i, o) => {
            let tag = match u.source {
                UsageSource::Reported => "reported",
                UsageSource::Measured => "measured",
                UsageSource::Estimated => "partial",
                UsageSource::Unknown => "unknown",
            };
            format!("{} in / {} out ({tag})", i.map(|x| x.to_string()).unwrap_or("?".into()), o.map(|x| x.to_string()).unwrap_or("?".into()))
        }
    }
}

/// Advisory limit check. Returns a warning only when the provider reported
/// numbers; unknown usage is never treated as "under budget".
pub fn budget_warning(total: &UsageRecord, max_cost: Option<f64>, max_tokens: Option<u64>) -> Option<String> {
    if let (Some(max), Some(c)) = (max_cost, total.cost_usd) {
        if c > max {
            return Some(format!("reported cost {} exceeds limits.max_cost_usd ${max:.2}", total.cost_display()));
        }
    }
    if let (Some(max), Some(i), Some(o)) = (max_tokens, total.input_tokens, total.output_tokens) {
        if i + o > max {
            return Some(format!("reported tokens {} exceed limits.max_tokens {max}", i + o));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_only_with_reported_numbers() {
        let u = UsageRecord { source: UsageSource::Reported, cost_usd: Some(3.0), input_tokens: Some(10), output_tokens: Some(5), ..Default::default() };
        assert!(budget_warning(&u, Some(2.0), None).is_some());
        assert!(budget_warning(&u, Some(5.0), Some(10)).is_some());
        assert!(budget_warning(&UsageRecord::unknown(None), Some(0.01), Some(1)).is_none());
        assert_eq!(tokens_display(&UsageRecord::unknown(None)), "tokens unknown");
        assert_eq!(tokens_display(&u), "10 in / 5 out (reported)");
    }
}
