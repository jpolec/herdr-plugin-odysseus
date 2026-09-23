//! Command normalization and semantic tagging for policy evaluation.
//!
//! This is *not* a shell parser. It canonicalizes the common ways a command
//! can be disguised (absolute program paths, env/sudo wrappers, git global
//! options, `sh -c` scripts, short-flag clusters) so rules match intent.
//! Unparseable shell constructs are handled conservatively: every
//! `&&`/`||`/`;`/`|`/newline segment is evaluated separately and the most
//! restrictive decision wins.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedCommand {
    /// Original argv as requested.
    pub argv: Vec<String>,
    /// Program basename after unwrapping wrappers (`git`, `cargo`…).
    pub program: String,
    /// Canonical single-spaced text: program + normalized args.
    pub text: String,
    /// True when executed through a shell (`shell: true` or `sh -c`).
    pub shell: bool,
    /// Normalized texts of each shell segment (empty for plain argv).
    pub segments: Vec<NormalizedCommand>,
    /// Semantic tags (see [`TAGS`]).
    pub tags: Vec<String>,
}

/// All semantic tags the classifier can emit.
pub const TAGS: &[&str] = &[
    "shell",
    "git",
    "git_push",
    "git_force_push",
    "git_reset_hard",
    "git_clean_force",
    "git_branch_force_delete",
    "git_history_rewrite",
    "rm_recursive",
    "rm_dangerous_target",
    "terraform_apply",
    "terraform_destroy",
    "kubectl_delete_namespace",
    "kubectl_mutation",
    "production_target",
    "deploy",
    "network",
    "credential_access",
    "package_install",
    "privilege_escalation",
];

impl NormalizedCommand {
    pub fn all_texts(&self) -> Vec<&str> {
        let mut v = vec![self.text.as_str()];
        for s in &self.segments {
            v.extend(s.all_texts());
        }
        v
    }
    pub fn all_tags(&self) -> Vec<&str> {
        let mut v: Vec<&str> = self.tags.iter().map(String::as_str).collect();
        for s in &self.segments {
            v.extend(s.all_tags());
        }
        v.sort_unstable();
        v.dedup();
        v
    }
}

fn basename(p: &str) -> String {
    p.rsplit('/').next().unwrap_or(p).to_string()
}

fn is_env_assignment(s: &str) -> bool {
    match s.split_once('=') {
        Some((k, _)) => {
            !k.is_empty()
                && k.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];

/// Normalize a command. `shell` means `argv` is `[script]` to run via `sh -c`.
pub fn normalize(argv: &[String], shell: bool) -> NormalizedCommand {
    if shell {
        let script = argv.join(" ");
        return normalize_script(&script, argv.to_vec());
    }
    normalize_argv(argv, 0)
}

fn normalize_script(script: &str, original: Vec<String>) -> NormalizedCommand {
    let segments: Vec<NormalizedCommand> = split_shell_segments(script)
        .into_iter()
        .map(|seg| normalize_argv(&tokenize(&seg), 1))
        .filter(|c| !c.program.is_empty())
        .collect();
    let mut tags = vec!["shell".to_string()];
    for s in &segments {
        for t in &s.tags {
            if !tags.contains(t) {
                tags.push(t.clone());
            }
        }
    }
    NormalizedCommand {
        argv: original,
        program: segments.first().map(|s| s.program.clone()).unwrap_or_default(),
        text: collapse_ws(script),
        shell: true,
        segments,
        tags,
    }
}

fn normalize_argv(argv: &[String], depth: u32) -> NormalizedCommand {
    let mut i = 0;
    let mut privileged = false;
    // Unwrap env assignments and common wrappers.
    while let Some(a) = argv.get(i) {
        let b = basename(a);
        if is_env_assignment(a) {
            i += 1;
        } else if b == "env" {
            i += 1;
            while argv.get(i).is_some_and(|x| x.starts_with('-') || is_env_assignment(x)) {
                i += 1;
            }
        } else if b == "sudo" || b == "doas" {
            privileged = true;
            i += 1;
            while let Some(x) = argv.get(i) {
                if x == "-u" || x == "-g" || x == "-C" || x == "-h" {
                    i += 2;
                } else if x.starts_with('-') {
                    i += 1;
                } else {
                    break;
                }
            }
        } else if matches!(b.as_str(), "nohup" | "time" | "command" | "exec" | "builtin") {
            i += 1;
        } else if b == "nice" {
            i += 1;
            if argv.get(i).is_some_and(|x| x == "-n") {
                i += 2;
            } else if argv.get(i).is_some_and(|x| x.starts_with('-')) {
                i += 1;
            }
        } else if b == "timeout" {
            i += 1;
            while argv.get(i).is_some_and(|x| x.starts_with('-')) {
                i += 1;
            }
            i += 1; // duration
        } else {
            break;
        }
    }
    let rest = &argv[i.min(argv.len())..];
    let Some(first) = rest.first() else {
        return NormalizedCommand {
            argv: argv.to_vec(),
            program: String::new(),
            text: String::new(),
            shell: false,
            segments: vec![],
            tags: vec![],
        };
    };
    let program = basename(first);
    let mut args: Vec<String> = rest[1..].to_vec();

    // `sh -c "script"` → evaluate the script too.
    if SHELLS.contains(&program.as_str()) && depth < 3 {
        if let Some(pos) = args.iter().position(|a| a == "-c" || (a.starts_with('-') && !a.starts_with("--") && a.contains('c'))) {
            if let Some(script) = args.get(pos + 1) {
                let mut inner = normalize_script(script, argv.to_vec());
                inner.text = collapse_ws(&format!("{program} -c {script}"));
                if privileged && !inner.tags.iter().any(|t| t == "privilege_escalation") {
                    inner.tags.push("privilege_escalation".into());
                }
                return inner;
            }
        }
    }

    if program == "git" {
        args = strip_git_global_options(&args);
    }
    let text = collapse_ws(&std::iter::once(program.clone()).chain(args.iter().cloned()).collect::<Vec<_>>().join(" "));
    let mut tags = classify(&program, &args);
    if privileged {
        tags.push("privilege_escalation".into());
    }
    NormalizedCommand {
        argv: argv.to_vec(),
        program,
        text,
        shell: false,
        segments: vec![],
        tags,
    }
}

fn strip_git_global_options(args: &[String]) -> Vec<String> {
    let mut out = vec![];
    let mut i = 0;
    let mut in_globals = true;
    while i < args.len() {
        let a = &args[i];
        if in_globals {
            if a == "-C" || a == "-c" || a == "--git-dir" || a == "--work-tree" || a == "--namespace" {
                i += 2;
                continue;
            }
            if a.starts_with("--git-dir=")
                || a.starts_with("--work-tree=")
                || a.starts_with("--namespace=")
                || a == "--no-pager"
                || a == "-P"
                || a == "--paginate"
                || a == "-p"
                || a == "--bare"
                || a == "--no-replace-objects"
                || a.starts_with("--exec-path")
            {
                i += 1;
                continue;
            }
            in_globals = false;
        }
        out.push(a.clone());
        i += 1;
    }
    out
}

/// Single-dash letter cluster (`-rf`) containing `flag`.
fn short_cluster_has(a: &str, flag: char) -> bool {
    a.len() >= 2
        && a.starts_with('-')
        && !a.starts_with("--")
        && a[1..].chars().all(|c| c.is_ascii_alphabetic())
        && a[1..].contains(flag)
}

fn has_long(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name || a.starts_with(&format!("{name}=")))
}

fn looks_production(a: &str) -> bool {
    let l = a.to_ascii_lowercase();
    l.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| w == "prod" || w == "production" || w == "prd")
}

const CREDENTIAL_MARKERS: &[&str] = &[
    ".ssh/id_",
    ".ssh/identity",
    ".aws/credentials",
    ".aws/config",
    ".netrc",
    ".config/gh/hosts.yml",
    ".docker/config.json",
    ".kube/config",
    ".gnupg",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    "/etc/shadow",
    "keychain",
];

fn classify(program: &str, args: &[String]) -> Vec<String> {
    let mut t: Vec<&str> = vec![];
    let sub = args.iter().find(|a| !a.starts_with('-')).map(String::as_str).unwrap_or("");
    match program {
        "git" => {
            t.push("git");
            match sub {
                "push" => {
                    t.push("git_push");
                    let forced = args.iter().any(|a| {
                        a == "--force"
                            || a.starts_with("--force-with-lease")
                            || a == "--force-if-includes"
                            || a == "--mirror"
                            || short_cluster_has(a, 'f')
                    }) || args
                        .iter()
                        .skip_while(|a| a.as_str() != "push")
                        .skip(1)
                        .any(|a| a.starts_with('+') || a.starts_with(":") && a.len() > 1)
                        || has_long(args, "--delete")
                        || args.iter().any(|a| a == "-d");
                    if forced {
                        t.push("git_force_push");
                    }
                }
                "reset" if args.iter().any(|a| a == "--hard" || a == "--merge" || a == "--keep") => {
                    t.push("git_reset_hard")
                }
                "clean" if args.iter().any(|a| a == "--force" || short_cluster_has(a, 'f')) => {
                    t.push("git_clean_force")
                }
                "branch" if args.iter().any(|a| a == "-D" || (short_cluster_has(a, 'D')) || (a == "--delete" && args.iter().any(|b| b == "--force"))) => {
                    t.push("git_branch_force_delete")
                }
                "filter-branch" | "filter-repo" => t.push("git_history_rewrite"),
                "rebase" | "commit" if args.iter().any(|a| a == "--amend" || a == "-i" || a == "--interactive") => {
                    t.push("git_history_rewrite")
                }
                "checkout" | "restore" if args.iter().any(|a| a == "--" || a == "." || a == "-f" || a == "--force") && sub == "checkout" && args.iter().any(|a| a == "-f" || a == "--force") => {
                    t.push("git_reset_hard")
                }
                _ => {}
            }
        }
        "rm" | "rmdir" | "unlink" => {
            let recursive = program == "rm"
                && args.iter().any(|a| {
                    a == "--recursive" || short_cluster_has(a, 'r') || short_cluster_has(a, 'R')
                });
            if recursive {
                t.push("rm_recursive");
            }
            let dangerous = args.iter().filter(|a| !a.starts_with('-')).any(|a| {
                let a = a.trim_end_matches('/');
                matches!(
                    a,
                    "" | "/" | "/*" | "~" | "~/*" | "$HOME" | "${HOME}" | "$HOME/*" | "." | ".." | "*" | "./*" | "../*" | ".git"
                ) || a.starts_with("/etc")
                    || a.starts_with("/usr")
                    || a.starts_with("/System")
                    || a.starts_with("/bin")
                    || a == "/Users"
                    || a == "/home"
            }) || args.iter().any(|a| a == "--no-preserve-root");
            if dangerous && (recursive || args.iter().any(|a| a == "--no-preserve-root")) {
                t.push("rm_dangerous_target");
            }
        }
        "terraform" | "tofu" | "terragrunt" => {
            if sub == "destroy" || (sub == "apply" && args.iter().any(|a| a == "-destroy" || a == "--destroy")) {
                t.push("terraform_destroy");
            } else if sub == "apply" || sub == "import" || sub == "state" {
                t.push("terraform_apply");
            }
        }
        "kubectl" | "oc" => {
            let mutating = matches!(
                sub,
                "apply" | "delete" | "create" | "replace" | "patch" | "scale" | "rollout" | "edit" | "drain" | "cordon" | "set" | "label" | "annotate"
            );
            if mutating {
                t.push("kubectl_mutation");
            }
            if sub == "delete"
                && args.iter().any(|a| matches!(a.as_str(), "ns" | "namespace" | "namespaces") || a.starts_with("namespace/") || a.starts_with("ns/"))
            {
                t.push("kubectl_delete_namespace");
            }
        }
        "helm" if matches!(sub, "install" | "upgrade" | "uninstall" | "delete" | "rollback") => {
            t.push("kubectl_mutation")
        }
        "curl" | "wget" | "nc" | "ncat" | "scp" | "rsync" | "ssh" | "sftp" | "ftp" | "telnet" => {
            t.push("network")
        }
        "npm" | "pnpm" | "yarn" | "bun" if matches!(sub, "install" | "i" | "add" | "ci") => {
            t.push("package_install")
        }
        "pip" | "pip3" | "uv" | "poetry" if args.iter().any(|a| a == "install" || a == "add") => {
            t.push("package_install")
        }
        "cargo" if sub == "install" => t.push("package_install"),
        "brew" | "apt" | "apt-get" | "dnf" | "yum" if matches!(sub, "install" | "upgrade") => {
            t.push("package_install")
        }
        _ => {}
    }
    if program.contains("deploy") || args.iter().any(|a| a == "deploy" || a.starts_with("deploy:")) {
        t.push("deploy");
    }
    let infra = matches!(program, "kubectl" | "oc" | "helm" | "terraform" | "tofu" | "terragrunt" | "aws" | "gcloud" | "az" | "flyctl" | "vercel" | "heroku" | "ansible-playbook")
        || t.contains(&"deploy");
    if infra && args.iter().any(|a| looks_production(a)) {
        t.push("production_target");
    }
    if args.iter().any(|a| CREDENTIAL_MARKERS.iter().any(|m| a.contains(m)))
        && !matches!(program, "git")
    {
        t.push("credential_access");
    }
    t.into_iter().map(String::from).collect()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Split a shell script into command segments on `&&`, `||`, `;`, `|`,
/// `&` and newlines outside quotes. Also extracts `$(…)` and backtick
/// substitutions as extra segments.
pub fn split_shell_segments(script: &str) -> Vec<String> {
    let mut segs = vec![];
    let mut cur = String::new();
    let mut chars = script.chars().peekable();
    let (mut sq, mut dq) = (false, false);
    let mut subs = vec![];
    while let Some(c) = chars.next() {
        match c {
            '\'' if !dq => {
                sq = !sq;
                cur.push(c);
            }
            '"' if !sq => {
                dq = !dq;
                cur.push(c);
            }
            '\\' if !sq => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '$' if !sq && chars.peek() == Some(&'(') => {
                chars.next();
                let mut depth = 1;
                let mut inner = String::new();
                for n in chars.by_ref() {
                    if n == '(' {
                        depth += 1;
                    } else if n == ')' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    inner.push(n);
                }
                subs.push(inner);
                cur.push_str("$(…)");
            }
            '`' if !sq => {
                let mut inner = String::new();
                for n in chars.by_ref() {
                    if n == '`' {
                        break;
                    }
                    inner.push(n);
                }
                subs.push(inner);
                cur.push_str("`…`");
            }
            ';' | '\n' | '|' | '&' if !sq && !dq => {
                if (c == '|' || c == '&') && chars.peek() == Some(&c) {
                    chars.next();
                }
                if !cur.trim().is_empty() {
                    segs.push(cur.trim().to_string());
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        segs.push(cur.trim().to_string());
    }
    for s in subs {
        segs.extend(split_shell_segments(&s));
    }
    segs
}

/// Minimal POSIX-ish tokenizer (quotes and backslashes) for policy purposes.
pub fn tokenize(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut has = false;
    let (mut sq, mut dq) = (false, false);
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !dq => {
                sq = !sq;
                has = true;
            }
            '"' if !sq => {
                dq = !dq;
                has = true;
            }
            '\\' if !sq => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                    has = true;
                }
            }
            c if c.is_whitespace() && !sq && !dq => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

/// Shell-quote an argv for *display* purposes only (never executed).
pub fn display_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:,@+%".contains(c)) {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    fn tags(argv: &[&str]) -> Vec<String> {
        normalize(&v(argv), false).all_tags().into_iter().map(String::from).collect()
    }

    #[test]
    fn unwraps_wrappers_and_paths() {
        let n = normalize(&v(&["FOO=1", "/usr/bin/env", "BAR=2", "sudo", "-u", "root", "/usr/bin/git", "-C", "/x", "-c", "a=b", "push", "--force"]), false);
        assert_eq!(n.program, "git");
        assert_eq!(n.text, "git push --force");
        assert!(n.tags.contains(&"git_force_push".into()));
        assert!(n.tags.contains(&"privilege_escalation".into()));
    }

    #[test]
    fn force_push_variants() {
        for argv in [
            &["git", "push", "-f"][..],
            &["git", "push", "origin", "main", "--force"],
            &["git", "push", "-uf", "origin", "x"],
            &["git", "push", "--force-with-lease"],
            &["git", "push", "origin", "+main"],
            &["git", "push", "origin", ":main"],
            &["git", "push", "--delete", "origin", "x"],
        ] {
            assert!(tags(argv).contains(&"git_force_push".to_string()), "{argv:?}");
        }
        assert!(!tags(&["git", "push", "--follow-tags"]).contains(&"git_force_push".to_string()));
        assert!(tags(&["git", "push", "origin", "herdr/1-x"]).contains(&"git_push".to_string()));
    }

    #[test]
    fn destructive_git() {
        assert!(tags(&["git", "reset", "--hard", "HEAD~1"]).contains(&"git_reset_hard".into()));
        assert!(tags(&["git", "clean", "-fdx"]).contains(&"git_clean_force".into()));
        assert!(!tags(&["git", "clean", "-n"]).contains(&"git_clean_force".into()));
        assert!(tags(&["git", "branch", "-D", "x"]).contains(&"git_branch_force_delete".into()));
    }

    #[test]
    fn rm_classification() {
        assert!(tags(&["rm", "-rf", "/"]).contains(&"rm_dangerous_target".into()));
        assert!(tags(&["rm", "-fr", "~"]).contains(&"rm_dangerous_target".into()));
        assert!(tags(&["rm", "-r", "--no-preserve-root", "/x"]).contains(&"rm_dangerous_target".into()));
        assert!(tags(&["rm", "-rf", "target"]).contains(&"rm_recursive".into()));
        assert!(!tags(&["rm", "-rf", "target"]).contains(&"rm_dangerous_target".into()));
        assert!(!tags(&["rm", "file.txt"]).contains(&"rm_recursive".into()));
    }

    #[test]
    fn shell_scripts_are_split() {
        let n = normalize(&v(&["cargo test && git push -f origin x; echo done"]), true);
        assert!(n.shell);
        assert_eq!(n.segments.len(), 3);
        assert!(n.all_tags().contains(&"git_force_push"));
        assert!(n.all_tags().contains(&"shell"));
        let n = normalize(&v(&["bash", "-lc", "echo $(rm -rf /)"]), false);
        assert!(n.all_tags().contains(&"rm_dangerous_target"), "{n:?}");
        let n = normalize(&v(&["sh", "-c", "echo 'a && git push -f' "]), false);
        // Quoted text is not a separate segment.
        assert!(!n.all_tags().contains(&"git_force_push"), "{n:?}");
    }

    #[test]
    fn infra_and_production() {
        assert!(tags(&["terraform", "destroy"]).contains(&"terraform_destroy".into()));
        assert!(tags(&["terraform", "apply", "-destroy"]).contains(&"terraform_destroy".into()));
        assert!(tags(&["terraform", "apply"]).contains(&"terraform_apply".into()));
        assert!(tags(&["kubectl", "delete", "namespace", "x"]).contains(&"kubectl_delete_namespace".into()));
        assert!(tags(&["kubectl", "--context", "prod-eu", "apply", "-f", "x"]).contains(&"production_target".into()));
        assert!(!tags(&["kubectl", "--context", "staging", "get", "pods"]).contains(&"production_target".into()));
        assert!(tags(&["./deploy.sh", "production"]).contains(&"production_target".into()));
        assert!(!tags(&["cargo", "test", "product"]).contains(&"production_target".into()));
    }

    #[test]
    fn credential_access() {
        assert!(tags(&["cat", "/Users/x/.aws/credentials"]).contains(&"credential_access".into()));
        assert!(tags(&["cp", "~/.ssh/id_ed25519", "/tmp"]).contains(&"credential_access".into()));
        assert!(!tags(&["cat", "README.md"]).contains(&"credential_access".into()));
    }

    #[test]
    fn tokenizer() {
        assert_eq!(tokenize(r#"a "b c" 'd e' f\ g"#), v(&["a", "b c", "d e", "f g"]));
        assert_eq!(tokenize("x ''"), v(&["x", ""]));
    }

    #[test]
    fn display_quoting() {
        assert_eq!(display_argv(&v(&["git", "commit", "-m", "it's"])), "git commit -m 'it'\\''s'");
    }
}
