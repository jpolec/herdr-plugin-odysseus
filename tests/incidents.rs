//! Sentry issues → reproduce-first fix tasks, through a fake `curl`.

mod common;

use common::*;
use herdr_orchestrator::model::*;

#[test]
fn sentry_issue_becomes_a_redacted_reproduce_first_task() {
    let h = Harness::new();
    let dir = h.dir.path();
    let issues = r#"[{"id":"4711","shortId":"BACKEND-1A","title":"ZeroDivisionError: division by zero","culprit":"billing.invoice in total","count":"37","userCount":5,"firstSeen":"2026-09-20T10:00:00Z","lastSeen":"2026-09-25T09:00:00Z","permalink":"https://sentry.io/organizations/acme/issues/4711/","level":"error"}]"#;
    let event = r#"{"tags":[{"key":"release","value":"api@1.4.2"},{"key":"environment","value":"production"},{"key":"user","value":"alice@example.com"}],
      "entries":[
        {"type":"exception","data":{"values":[{"type":"ZeroDivisionError","value":"division by zero (api_key=sk_live_abcdefghijklmnop1234)","stacktrace":{"frames":[
          {"filename":"site-packages/flask/app.py","lineNo":10,"function":"dispatch","inApp":false},
          {"filename":"billing/invoice.py","lineNo":42,"function":"total","inApp":true}]}}]}},
        {"type":"request","data":{"url":"https://api/x","headers":[["Cookie","session=SECRET-COOKIE"]]}},
        {"type":"breadcrumbs","data":{"values":[{"message":"SELECT * FROM users WHERE email='alice@example.com'"}]}}],
      "user":{"email":"alice@example.com"}}"#;
    std::fs::write(dir.join("issues.json"), issues).unwrap();
    std::fs::write(dir.join("event.json"), event).unwrap();
    let curl = dir.join("fake-curl");
    std::fs::write(
        &curl,
        format!(
            "#!/bin/sh\necho \"$@\" >> {log}\ncfg=$(cat)\necho \"$cfg\" > {cfglog}\ncase \"$cfg\" in\n  *events/latest*) cat {event};;\n  *projects/acme/api/issues*) cat {issues};;\n  *) exit 22;;\nesac\n",
            log = dir.join("curl.log").display(),
            cfglog = dir.join("curl.cfg").display(),
            event = dir.join("event.json").display(),
            issues = dir.join("issues.json").display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::env::set_var("HERDR_ORCH_CURL_BIN", &curl);
    std::env::set_var("SENTRY_AUTH_TOKEN", "sntrys_TESTTOKEN");
    h.project_file("config.yaml", "sentry:\n  org: acme\n  project: api\n");

    let cfg = h.ctx.load_config(Some(&h.repo)).unwrap().config.sentry;
    let list = herdr_orchestrator::engine::incidents::list(&cfg, "24h").unwrap();
    assert_eq!(list[0].short_id, "BACKEND-1A");
    let t = herdr_orchestrator::engine::incidents::start(&h.ctx, &h.repo, "backend-1a", Some("fake-success".into()), None).unwrap();
    assert_eq!(t.options.workflow.as_deref(), Some("contract-first"));
    assert!(matches!(&t.source, Some(TaskSource::Incident { id, .. }) if id == "4711"));
    let d = &t.description;
    assert!(d.contains("billing/invoice.py:42") && d.contains("at total"), "{d}");
    assert!(!d.contains("flask/app.py"), "only in-app frames");
    assert!(d.contains("release: api@1.4.2") && d.contains("environment: production"));
    for leak in ["alice@example.com", "SECRET-COOKIE", "SELECT *", "sk_live_abcdefghijklmnop1234"] {
        assert!(!d.contains(leak), "leaked {leak}: {d}");
    }
    assert!(d.contains("First reproduce it"));
    assert_eq!(t.acceptance.len(), 1);
    // The token never appears in curl's arguments.
    let argv = std::fs::read_to_string(dir.join("curl.log")).unwrap();
    assert!(!argv.contains("TESTTOKEN"), "{argv}");
    assert!(std::fs::read_to_string(dir.join("curl.cfg")).unwrap().contains("Bearer sntrys_TESTTOKEN"));
    // One open task per incident.
    assert!(herdr_orchestrator::engine::incidents::start(&h.ctx, &h.repo, "4711", None, None).is_err());
}
