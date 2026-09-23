//! `doctor`: local environment checks. No network calls unless `--network`.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use clap::Args;
use serde::Serialize;

use super::App;
use crate::herdr::{HerdrApi, SocketHerdr};

#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// Also run checks that talk to the network (`gh auth status`).
    #[arg(long)]
    pub network: bool,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Serialize)]
struct Check {
    level: Level,
    name: String,
    detail: String,
}

fn tool_version(bin: &str, args: &[&str]) -> Option<String> {
    let p = crate::process::which(bin)?;
    let mut argv = vec![p.to_string_lossy().to_string()];
    argv.extend(args.iter().map(|s| s.to_string()));
    let out = crate::process::run(&crate::process::Spec::new(argv, Path::new(".")).timeout(Duration::from_secs(10)), None).ok()?;
    let s = format!("{}{}", out.stdout_str(), out.stderr_str());
    Some(s.lines().next().unwrap_or("").trim().to_string())
}

pub fn run(app: &App, a: DoctorArgs) -> Result<i32> {
    let mut checks: Vec<Check> = vec![];
    let mut add = |level: Level, name: &str, detail: String| checks.push(Check { level, name: name.into(), detail });

    // Herdr
    let herdr_bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    match tool_version(&herdr_bin, &["--version"]) {
        Some(v) => {
            let ver = v.split_whitespace().last().unwrap_or("").to_string();
            let ok = semver_ge(&ver, "0.9.0");
            add(if ok { Level::Ok } else { Level::Error }, "herdr binary", format!("{v}{}", if ok { "" } else { " (need >= 0.9.0)" }));
        }
        None => add(Level::Warn, "herdr binary", "not found; agent panes unavailable (headless runners still work)".into()),
    }
    let cfg = crate::config::load(&app.paths, app.repo_opt().as_deref(), None);
    let socket_cfg = cfg.as_ref().ok().and_then(|c| c.config.herdr.socket.clone());
    match SocketHerdr::discover(socket_cfg.as_deref()) {
        Some(h) => match h.ping() {
            Ok(p) => add(Level::Ok, "herdr socket", format!("{} (server {} protocol {})", h.client.path.display(), p.version, p.protocol)),
            Err(e) => add(Level::Warn, "herdr socket", format!("{}: {e}", h.client.path.display())),
        },
        None => add(Level::Warn, "herdr socket", "no socket found (is Herdr running?)".into()),
    }
    // Plugin registration (read-only CLI query).
    if crate::process::which(&herdr_bin).is_some() {
        let out = crate::process::run(&crate::process::Spec::new(vec![herdr_bin.clone(), "plugin".into(), "list".into(), "--json".into()], Path::new(".")).timeout(Duration::from_secs(10)), None);
        match out {
            Ok(o) if o.stdout_str().contains(crate::config::PLUGIN_ID) => add(Level::Ok, "plugin registration", format!("{} is registered", crate::config::PLUGIN_ID)),
            Ok(_) => add(Level::Warn, "plugin registration", format!("{} not registered; run `herdr plugin link <repo>` or install it", crate::config::PLUGIN_ID)),
            Err(e) => add(Level::Warn, "plugin registration", format!("{e:#}")),
        }
    }
    // Tools
    match tool_version("git", &["--version"]) {
        Some(v) => add(Level::Ok, "git", v),
        None => add(Level::Error, "git", "not found (required)".into()),
    }
    let gh = crate::github::Gh::from_env();
    match tool_version(&gh.bin.to_string_lossy(), &["--version"]) {
        Some(v) => add(Level::Ok, "gh", v),
        None => add(Level::Warn, "gh", "not found; github_pr steps and --from-issue unavailable".into()),
    }
    if a.network && gh.available() {
        match tool_version(&gh.bin.to_string_lossy(), &["auth", "status"]) {
            Some(v) => add(Level::Ok, "gh auth", v),
            None => add(Level::Warn, "gh auth", "not authenticated".into()),
        }
    }
    for (bin, args) in [("claude", vec!["--version"]), ("codex", vec!["--version"]), ("opencode", vec!["--version"])] {
        match tool_version(bin, &args) {
            Some(v) => add(Level::Ok, bin, v),
            None => add(Level::Warn, bin, "not found on PATH (runner unavailable)".into()),
        }
    }
    // Repo
    match app.repo() {
        Ok(r) => {
            add(Level::Ok, "repository", r.display().to_string());
            match crate::git::is_dirty(&r) {
                Ok(true) => add(Level::Warn, "repository state", "uncommitted changes in the main checkout (runs use their own worktrees; base is the committed ref)".into()),
                Ok(false) => add(Level::Ok, "repository state", "clean".into()),
                Err(e) => add(Level::Warn, "repository state", format!("{e:#}")),
            }
        }
        Err(e) => add(Level::Warn, "repository", format!("{e:#}")),
    }
    // Config
    match &cfg {
        Ok(c) => {
            add(Level::Ok, "config", format!("{} layer(s): {}", c.layers.len(), c.layers.iter().map(|l| l.name.as_str()).collect::<Vec<_>>().join(" → ")));
            let ctx = app.ctx(false)?;
            match ctx.policy_for(c) {
                Ok(p) => add(Level::Ok, "policy", format!("{} rules from {}", p.rule_count(), p.sources.join(", "))),
                Err(e) => add(Level::Error, "policy", format!("{e:#}")),
            }
            let cat = ctx.catalog(app.repo_opt().as_deref());
            match cat.workflows() {
                Ok(ws) => {
                    let bad: Vec<String> = ws.iter().filter_map(|w| crate::workflow::Workflow::parse(&w.yaml).err().map(|e| format!("{}: {e:#}", w.name))).collect();
                    if bad.is_empty() {
                        add(Level::Ok, "workflows", format!("{} valid", ws.len()));
                    } else {
                        add(Level::Error, "workflows", bad.join("; "));
                    }
                }
                Err(e) => add(Level::Error, "workflows", format!("{e:#}")),
            }
            let factory = crate::engine::runner_factory(&ctx, c);
            if let Err(e) = factory.profile(&c.config.defaults.runner) {
                add(Level::Error, "default runner", format!("{e:#}"));
            }
        }
        Err(e) => add(Level::Error, "config", format!("{e:#}")),
    }
    // State dir
    let st = &app.paths.state_dir;
    match crate::store::Store::open(st) {
        Ok(store) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(st).map(|m| m.permissions().mode() & 0o777).unwrap_or(0);
                if mode & 0o077 != 0 {
                    add(Level::Warn, "state permissions", format!("{} is {:o}; expected 700", st.display(), mode));
                } else {
                    add(Level::Ok, "state dir", format!("{} ({})", st.display(), app.paths.state_source));
                }
            }
            let corrupt = store.corrupt_files();
            if corrupt.is_empty() {
                add(Level::Ok, "state integrity", "no quarantined files".into());
            } else {
                add(Level::Error, "state integrity", format!("{} quarantined corrupt file(s): {}", corrupt.len(), corrupt.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")));
            }
            if let Ok(env_dir) = std::env::var("HERDR_PLUGIN_STATE_DIR") {
                if Path::new(&env_dir) != st {
                    add(Level::Warn, "state location", format!("HERDR_PLUGIN_STATE_DIR={env_dir} differs from {}", st.display()));
                }
            }
            add(
                if crate::daemon::is_running(&store.layout) { Level::Ok } else { Level::Warn },
                "daemon",
                if crate::daemon::is_running(&store.layout) { format!("running (pid {})", crate::daemon::daemon_pid(&store.layout).unwrap_or(0)) } else { "not running (started on demand)".into() },
            );
        }
        Err(e) => add(Level::Error, "state dir", format!("{}: {e:#}", st.display())),
    }
    add(Level::Ok, "telemetry", "none — no data leaves this machine".into());

    let worst = checks.iter().map(|c| c.level).max().unwrap_or(Level::Ok);
    if app.cli_json {
        app.print_json(&checks)?;
    } else {
        for c in &checks {
            let tag = match c.level {
                Level::Ok => "OK   ",
                Level::Warn => "WARN ",
                Level::Error => "ERROR",
            };
            println!("{tag} {:<20} {}", c.name, c.detail);
        }
    }
    Ok(if worst == Level::Error { 1 } else { 0 })
}

fn semver_ge(a: &str, b: &str) -> bool {
    let p = |s: &str| s.split(['.', '-']).take(3).map(|x| x.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    p(a) >= p(b)
}

#[cfg(test)]
mod tests {
    #[test]
    fn semver() {
        assert!(super::semver_ge("0.9.0", "0.9.0"));
        assert!(super::semver_ge("0.10.1", "0.9.0"));
        assert!(!super::semver_ge("0.8.9", "0.9.0"));
    }
}
