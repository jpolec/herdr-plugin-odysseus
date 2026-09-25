//! Project memory: prior work, notes and active runs in agent prompts.

mod common;

use common::*;
use herdr_orchestrator::model::*;

/// The prompt a fake agent received (fake transcripts echo it into the log).
fn prompt_of(r: &Run, step: &str) -> String {
    let e = r.steps.iter().find(|e| e.step_id == step).unwrap();
    std::fs::read_to_string(e.log_path.as_ref().unwrap()).unwrap_or_default()
}

fn scoped(h: &Harness, text: &str, runner: &str, scope: &str, workflow: &str) -> Task {
    h.task_with(text, TaskOptions { workflow: Some(workflow.into()), runner: Some(runner.into()), scope: vec![scope.into()], ..Default::default() })
}

#[test]
fn prior_work_notes_and_failures_reach_the_next_agent_but_not_reviewers() {
    let mut h = Harness::new();
    // 1. An agent changes fake/ and leaves a note for others.
    let a = scoped(&h, "Normalize FX rates in fake/fx.rs", "fake-leave-note", "fake/**", "quick-task");
    assert_eq!(h.settle(&a.task_id)[0].status, RunStatus::Succeeded);
    // 2. A failed attempt in the same area.
    let b = scoped(&h, "FX rounding in fake/fx.rs", "fake-fail", "fake/**", "quick-task");
    assert_eq!(h.settle(&b.task_id)[0].status, RunStatus::Failed);
    // 3. A human note on the area and one elsewhere.
    for (text, scope) in [("Never round FX before aggregation", "fake/**"), ("Docs use British spelling", "docs/**")] {
        herdr_orchestrator::memory::add_note(&h.ctx, herdr_orchestrator::memory::Note { id: new_id("note"), repo: h.repo.clone(), text: text.into(), scope: vec![scope.into()], author: "human:tester".into(), at: now(), run: None, task_id: None }).unwrap();
    }
    // 4. Unrelated work.
    let u = h.task("Rewrite the logging banner", "quick-task", Some("fake-fail"));
    h.settle(&u.task_id);

    h.workflow("rev", "  - id: implement\n    type: agent\n    prompt: \"{{task}}\"\n  - id: review\n    type: agent\n    output: review\n    prompt: review it\n");
    let c = h.task_with("Fix FX conversion for NAV totals in fake/fx.rs", TaskOptions { workflow: Some("rev".into()), step_runners: [("implement".to_string(), "fake-success".to_string()), ("review".to_string(), "fake-review-approve".to_string())].into(), scope: vec!["fake/**".into()], ..Default::default() });
    let r = h.settle(&c.task_id)[0].clone();
    let p = prompt_of(&r, "implement");
    assert!(p.contains("## Prior work in this repository"), "{p}");
    assert!(p.contains("NAV currency is canonical"), "agent note: {p}");
    assert!(p.contains("Never round FX before aggregation"), "human note: {p}");
    assert!(p.contains("failed") && p.contains(&format!("#{}", b.task_id)), "failure with reference: {p}");
    assert!(p.contains(&format!("#{}", a.task_id)));
    assert!(!p.contains("British spelling") && !p.contains("logging banner"), "irrelevant history stays out: {p}");
    assert!(!prompt_of(&r, "review").contains("Prior work"), "reviewers judge independently");
    assert!(h.events(&r.run_id).contains(&"memory_injected".into()));
    // The agent's note was stored with the files it changed as scope.
    let notes = herdr_orchestrator::memory::load_notes(&h.ctx);
    let agent_note = notes.iter().find(|n| n.author.starts_with("agent:")).unwrap();
    assert_eq!(agent_note.scope, vec!["fake/fx.rs"]);
}

#[test]
fn memory_can_be_switched_off() {
    let mut h = Harness::new();
    let a = scoped(&h, "Touch fake files", "fake-success", "fake/**", "quick-task");
    h.settle(&a.task_id);
    h.project_file("config.yaml", "memory:\n  history: false\n");
    let b = scoped(&h, "Touch fake files again", "fake-success", "fake/**", "quick-task");
    let r = h.settle(&b.task_id)[0].clone();
    assert!(!prompt_of(&r, "implement").contains("Prior work"));
}

#[test]
fn history_preview_matches_what_the_agent_gets() {
    let mut h = Harness::new();
    let a = scoped(&h, "Add FX cache in fake/fx.rs", "fake-success", "fake/**", "quick-task");
    h.settle(&a.task_id);
    let t = h.task_with("Speed up the FX cache", TaskOptions { workflow: Some("quick-task".into()), runner: Some("fake-success".into()), scope: vec!["fake/**".into()], ..Default::default() });
    let records = herdr_orchestrator::memory::collect(&h.ctx, &h.repo).unwrap();
    let hits = herdr_orchestrator::memory::rank(&records, &herdr_orchestrator::memory::query_for(&t), now(), 8);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].record.run.as_deref(), Some(format!("#{}", a.task_id).as_str()));
    assert!(hits[0].why.contains("same area") || hits[0].why.contains("same files"), "{}", hits[0].why);
}
