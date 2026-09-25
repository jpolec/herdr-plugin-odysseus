//! Token usage of interactive (pane) agents, read from the agents' own
//! local session logs. Nothing is sent anywhere; the files are only read.
//!
//! Pane agents keep one session across retries and several steps, so usage
//! is attributed by *time window*: only activity between a step's start and
//! end counts towards that step.
//!
//! - Claude Code writes `<config>/projects/<slug>/<session_id>.jsonl`
//!   (config = `$CLAUDE_CONFIG_DIR` or `~/.claude`), one line per streamed
//!   assistant message chunk; the same `message.id` repeats with the same
//!   `usage`, so lines are de-duplicated by message id. Subagent transcripts
//!   live in `<slug>/<session_id>/subagents/*.jsonl` and count too.
//! - Codex writes `<home>/sessions/YYYY/MM/DD/rollout-<ts>-<session_id>.jsonl`
//!   (home = `$CODEX_HOME` or `~/.codex`) with cumulative `token_count`
//!   events; a step's usage is the growth of the running total in its window.
//!
//! Provenance: when Herdr reported the agent's native session id the numbers
//! are `reported`; when the file had to be found by working directory they
//! are `estimated` (the attribution, not the arithmetic, is uncertain).
//! Neither log carries a price, so `cost_usd` stays unknown.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::model::{UsageRecord, UsageSource};

/// Largest session log we are willing to scan (bytes).
const MAX_LOG_BYTES: u64 = 512 * 1024 * 1024;

/// Usage of a pane agent between `since` and `until`. `None` when the agent
/// kind keeps no readable log or no matching log exists.
pub fn pane_usage(
    agent_kind: &str,
    session_id: Option<&str>,
    worktree: &Path,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
) -> Option<UsageRecord> {
    match agent_kind {
        "claude" => claude_usage(&claude_home()?, session_id, worktree, since, until),
        "codex" => codex_usage(&codex_home()?, session_id, worktree, since, until),
        _ => None,
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

pub fn claude_home() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).or_else(|| home().map(|h| h.join(".claude")))
}

pub fn codex_home() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME").map(PathBuf::from).or_else(|| home().map(|h| h.join(".codex")))
}

/// Claude's project directory name for a working directory: every
/// character other than an ASCII letter or digit becomes `-`.
pub fn claude_project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn lines_of(path: &Path) -> Option<impl Iterator<Item = String>> {
    let md = std::fs::metadata(path).ok()?;
    if !md.is_file() || md.len() > MAX_LOG_BYTES {
        return None;
    }
    let f = std::fs::File::open(path).ok()?;
    Some(BufReader::new(f).lines().map_while(Result::ok))
}

fn timestamp(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    v.get("timestamp")?.as_str()?.parse::<DateTime<Utc>>().ok()
}

fn in_window(t: DateTime<Utc>, since: DateTime<Utc>, until: DateTime<Utc>) -> bool {
    t >= since && t <= until
}

// ------------------------------------------------------------------ claude

fn claude_usage(home: &Path, session_id: Option<&str>, worktree: &Path, since: DateTime<Utc>, until: DateTime<Utc>) -> Option<UsageRecord> {
    // If the id Herdr reported is not a native session id (or its log is
    // gone), fall back to the worktree's own logs as an estimate.
    claude_usage_once(home, session_id, worktree, since, until).or_else(|| session_id.and_then(|_| claude_usage_once(home, None, worktree, since, until)))
}

fn claude_usage_once(home: &Path, session_id: Option<&str>, worktree: &Path, since: DateTime<Utc>, until: DateTime<Utc>) -> Option<UsageRecord> {
    let projects = home.join("projects");
    let (files, source) = match session_id.filter(|s| safe_id(s)) {
        Some(id) => {
            let mut files = vec![];
            for dir in std::fs::read_dir(&projects).ok()?.flatten() {
                let main = dir.path().join(format!("{id}.jsonl"));
                if main.is_file() {
                    files.push(main);
                    files.extend(jsonl_files(&dir.path().join(id).join("subagents")));
                }
            }
            (files, UsageSource::Reported)
        }
        None => {
            // No session id: every session started in this worktree. The
            // worktree is private to one run, so this is still the run's
            // agent; only per-step attribution is less certain.
            let dir = projects.join(claude_project_slug(worktree));
            let mut files = jsonl_files(&dir);
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for d in rd.flatten().filter(|d| d.path().is_dir()) {
                    files.extend(jsonl_files(&d.path().join("subagents")));
                }
            }
            (files, UsageSource::Estimated)
        }
    };
    if files.is_empty() {
        return None;
    }
    let wt = worktree.to_string_lossy().to_string();
    // message id → (input, output, cache_read, model); the last line wins.
    let mut by_msg: HashMap<String, (u64, u64, u64, Option<String>)> = HashMap::new();
    for f in &files {
        for line in lines_of(f).into_iter().flatten() {
            if !line.contains("\"usage\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
            if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
                continue;
            }
            let Some(t) = timestamp(&v) else { continue };
            if !in_window(t, since, until) {
                continue;
            }
            if source == UsageSource::Estimated {
                // Found by directory: only trust lines that say they ran here.
                match v.get("cwd").and_then(|c| c.as_str()) {
                    Some(c) if c == wt || c.starts_with(&format!("{wt}/")) => {}
                    _ => continue,
                }
            }
            let m = &v["message"];
            let u = &m["usage"];
            let g = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
            let id = m.get("id").and_then(|x| x.as_str()).map(String::from).unwrap_or_else(|| format!("line:{}", by_msg.len()));
            by_msg.insert(
                id,
                (
                    g("input_tokens") + g("cache_creation_input_tokens"),
                    g("output_tokens"),
                    g("cache_read_input_tokens"),
                    m.get("model").and_then(|x| x.as_str()).map(String::from),
                ),
            );
        }
    }
    if by_msg.is_empty() {
        return None;
    }
    let mut u = UsageRecord { source, input_tokens: Some(0), output_tokens: Some(0), cached_tokens: Some(0), ..Default::default() };
    for (i, o, c, model) in by_msg.into_values() {
        *u.input_tokens.as_mut().unwrap() += i;
        *u.output_tokens.as_mut().unwrap() += o;
        *u.cached_tokens.as_mut().unwrap() += c;
        if model.is_some() {
            u.model = model;
        }
    }
    Some(u)
}

fn jsonl_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl")).collect())
        .unwrap_or_default();
    v.sort();
    v
}

// ------------------------------------------------------------------- codex

fn codex_usage(home: &Path, session_id: Option<&str>, worktree: &Path, since: DateTime<Utc>, until: DateTime<Utc>) -> Option<UsageRecord> {
    codex_usage_once(home, session_id, worktree, since, until).or_else(|| session_id.and_then(|_| codex_usage_once(home, None, worktree, since, until)))
}

fn codex_usage_once(home: &Path, session_id: Option<&str>, worktree: &Path, since: DateTime<Utc>, until: DateTime<Utc>) -> Option<UsageRecord> {
    let sessions = home.join("sessions");
    // Rollout files are grouped by (local) start day; a step can only have
    // used a session that started on or before `until`, and sessions rarely
    // span more than a few days, so a week of day-directories is enough.
    // Start one day late to cover time zones ahead of UTC.
    let mut candidates = vec![];
    let mut day = until.date_naive().succ_opt()?;
    for _ in 0..8 {
        let dir = sessions.join(day.format("%Y/%m/%d").to_string());
        candidates.extend(jsonl_files(&dir));
        day = day.pred_opt()?;
    }
    let (files, source): (Vec<PathBuf>, UsageSource) = match session_id.filter(|s| safe_id(s)) {
        Some(id) => (candidates.into_iter().filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(&format!("{id}.jsonl")))).collect(), UsageSource::Reported),
        None => {
            let wt = worktree.to_string_lossy().to_string();
            (candidates.into_iter().filter(|p| codex_session_cwd(p).as_deref() == Some(wt.as_str())).collect(), UsageSource::Estimated)
        }
    };
    let mut total = UsageRecord { source, input_tokens: Some(0), output_tokens: Some(0), cached_tokens: Some(0), ..Default::default() };
    let mut any = false;
    for f in files {
        if let Some((i, o, c, model)) = codex_file_delta(&f, since, until) {
            any = true;
            *total.input_tokens.as_mut().unwrap() += i;
            *total.output_tokens.as_mut().unwrap() += o;
            *total.cached_tokens.as_mut().unwrap() += c;
            if model.is_some() {
                total.model = model;
            }
        }
    }
    any.then_some(total)
}

fn codex_session_cwd(p: &Path) -> Option<String> {
    let line = lines_of(p)?.next()?;
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
        return None;
    }
    v.pointer("/payload/cwd").and_then(|c| c.as_str()).map(String::from)
}

/// Growth of the cumulative token counters inside the window:
/// (input incl. cached, output, cached input, model).
fn codex_file_delta(p: &Path, since: DateTime<Utc>, until: DateTime<Utc>) -> Option<(u64, u64, u64, Option<String>)> {
    let mut before = (0u64, 0u64, 0u64);
    let mut last: Option<(u64, u64, u64)> = None;
    let mut model = None;
    for line in lines_of(p)? {
        if !line.contains("token_count") && !line.contains("turn_context") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        let Some(t) = timestamp(&v) else { continue };
        if v.get("type").and_then(|x| x.as_str()) == Some("turn_context") {
            if t <= until {
                if let Some(m) = v.pointer("/payload/model").and_then(|m| m.as_str()) {
                    model = Some(m.to_string());
                }
            }
            continue;
        }
        let Some(tot) = v.pointer("/payload/info/total_token_usage") else { continue };
        let g = |k: &str| tot.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        let cur = (g("input_tokens"), g("output_tokens"), g("cached_input_tokens"));
        if t < since {
            before = cur;
        } else if t <= until {
            last = Some(cur);
        }
    }
    let end = last?;
    Some((end.0.saturating_sub(before.0), end.1.saturating_sub(before.1), end.2.saturating_sub(before.2), model))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn claude_line(t: &str, id: &str, cwd: &str, input: u64, out: u64) -> String {
        serde_json::json!({
            "type": "assistant", "timestamp": t, "cwd": cwd,
            "message": {"id": id, "model": "claude-x", "usage": {"input_tokens": input, "cache_creation_input_tokens": 10, "cache_read_input_tokens": 100, "output_tokens": out}}
        })
        .to_string()
    }

    #[test]
    fn claude_dedupes_chunks_and_respects_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let wt = Path::new("/work/repo/.herdr-orchestrator/worktrees/7-x");
        let proj = dir.path().join("projects").join(claude_project_slug(wt));
        std::fs::create_dir_all(proj.join("sess-1/subagents")).unwrap();
        let cwd = wt.to_str().unwrap();
        let main = [
            claude_line("2026-01-01T10:00:00Z", "m0", cwd, 999, 999), // before the step
            claude_line("2026-01-01T10:05:00Z", "m1", cwd, 5, 20),
            claude_line("2026-01-01T10:05:01Z", "m1", cwd, 5, 20), // same message, next chunk
            claude_line("2026-01-01T10:06:00Z", "m2", cwd, 7, 30),
            "{not json".to_string(),
            serde_json::json!({"type": "user", "timestamp": "2026-01-01T10:06:30Z"}).to_string(),
        ];
        std::fs::write(proj.join("sess-1.jsonl"), main.join("\n")).unwrap();
        std::fs::write(proj.join("sess-1/subagents/agent-a.jsonl"), claude_line("2026-01-01T10:07:00Z", "s1", cwd, 1, 2)).unwrap();
        let (since, until) = (ts("2026-01-01T10:01:00Z"), ts("2026-01-01T10:10:00Z"));
        let u = claude_usage(dir.path(), Some("sess-1"), wt, since, until).unwrap();
        assert_eq!(u.source, UsageSource::Reported);
        assert_eq!(u.input_tokens, Some((5 + 10) + (7 + 10) + (1 + 10)));
        assert_eq!(u.output_tokens, Some(20 + 30 + 2));
        assert_eq!(u.cached_tokens, Some(300));
        assert_eq!(u.model.as_deref(), Some("claude-x"));
        assert_eq!(u.cost_usd, None, "session logs carry no price");
        // Without a session id the file is found by directory; lines from
        // another cwd are ignored and the result is only an estimate.
        std::fs::write(proj.join("other.jsonl"), claude_line("2026-01-01T10:05:00Z", "x9", "/elsewhere", 1000, 1000)).unwrap();
        let e = claude_usage(dir.path(), None, wt, since, until).unwrap();
        assert_eq!(e.source, UsageSource::Estimated);
        assert_eq!(e.output_tokens, Some(52));
        // An empty window gives nothing; a path-like id is never used as a
        // path (the directory fallback applies).
        // An id that matches no log falls back to the worktree's logs.
        assert_eq!(claude_usage(dir.path(), Some("nope"), wt, since, until).unwrap().source, UsageSource::Estimated);
        assert_eq!(claude_usage(dir.path(), Some("../x"), wt, since, until).unwrap().source, UsageSource::Estimated);
        assert!(claude_usage(dir.path(), Some("sess-1"), wt, ts("2027-01-01T00:00:00Z"), ts("2027-01-02T00:00:00Z")).is_none());
    }

    #[test]
    fn codex_uses_growth_of_the_running_total() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("sessions/2026/01/01");
        std::fs::create_dir_all(&day).unwrap();
        let tc = |t: &str, i: u64, o: u64, c: u64| serde_json::json!({"timestamp": t, "type": "event_msg", "payload": {"type": "token_count", "info": {"total_token_usage": {"input_tokens": i, "output_tokens": o, "cached_input_tokens": c}}}}).to_string();
        let lines = [
            serde_json::json!({"timestamp": "2026-01-01T09:00:00Z", "type": "session_meta", "payload": {"id": "abc-1", "cwd": "/wt"}}).to_string(),
            serde_json::json!({"timestamp": "2026-01-01T09:00:01Z", "type": "turn_context", "payload": {"model": "gpt-x"}}).to_string(),
            tc("2026-01-01T09:30:00Z", 1000, 100, 500),
            tc("2026-01-01T10:10:00Z", 1500, 160, 700),
            tc("2026-01-01T10:20:00Z", 2500, 260, 900),
            tc("2026-01-01T12:00:00Z", 9000, 900, 5000),
        ];
        std::fs::write(day.join("rollout-2026-01-01T09-00-00-abc-1.jsonl"), lines.join("\n")).unwrap();
        let (since, until) = (ts("2026-01-01T10:00:00Z"), ts("2026-01-01T11:00:00Z"));
        let u = codex_usage(dir.path(), Some("abc-1"), Path::new("/wt"), since, until).unwrap();
        assert_eq!((u.input_tokens, u.output_tokens, u.cached_tokens), (Some(1500), Some(160), Some(400)));
        assert_eq!(u.source, UsageSource::Reported);
        assert_eq!(u.model.as_deref(), Some("gpt-x"));
        let e = codex_usage(dir.path(), None, Path::new("/wt"), since, until).unwrap();
        assert_eq!(e.source, UsageSource::Estimated);
        assert!(codex_usage(dir.path(), None, Path::new("/other"), since, until).is_none());
    }

    #[test]
    fn slug_matches_claude_layout() {
        assert_eq!(claude_project_slug(Path::new("/Users/j/_Work/a.b")), "-Users-j--Work-a-b");
    }
}
