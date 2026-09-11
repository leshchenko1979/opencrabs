use crate::brain::agent::service::AgentService;
use crate::brain::agent::service::context::parse_context_manifest;
use std::collections::HashSet;

#[test]
fn test_parse_context_manifest_yaml() {
    let summary = r#"
## 0. IMMEDIATE TASK
CONTINUE THIS TASK: Do something.

## 10. Context Manifest
```context-manifest
active_skills:
  - opencrabs-dev
  - coding-process
discard_skills:
  - grill-for-unknowns
required_tools:
  - telegram_send
  - browser_navigate
```
"#;

    let manifest = parse_context_manifest(summary).expect("should parse manifest");
    assert_eq!(
        manifest.active_skills,
        vec!["opencrabs-dev".to_string(), "coding-process".to_string()]
    );
    assert_eq!(
        manifest.discard_skills,
        vec!["grill-for-unknowns".to_string()]
    );
    assert_eq!(
        manifest.required_tools,
        vec!["telegram_send".to_string(), "browser_navigate".to_string()]
    );
}

#[test]
fn test_parse_context_manifest_json_fallback() {
    let summary = r#"
## 10. Context Manifest
```context-manifest
{
  "active_skills": ["meta-factory"],
  "discard_skills": ["a2a-gateway"],
  "required_tools": ["cron_manage"]
}
```
"#;

    let manifest = parse_context_manifest(summary).expect("should parse json manifest");
    assert_eq!(manifest.active_skills, vec!["meta-factory".to_string()]);
    assert_eq!(manifest.discard_skills, vec!["a2a-gateway".to_string()]);
    assert_eq!(manifest.required_tools, vec!["cron_manage".to_string()]);
}

#[test]
fn test_parse_context_manifest_inline_flow_yaml() {
    let summary = r#"
```context-manifest
active_skills: [opencrabs-dev, github]
discard_skills: []
required_tools: [telegram_send]
```
"#;

    let manifest = parse_context_manifest(summary).expect("should parse inline yaml");
    assert_eq!(
        manifest.active_skills,
        vec!["opencrabs-dev".to_string(), "github".to_string()]
    );
    assert!(manifest.discard_skills.is_empty());
    assert_eq!(manifest.required_tools, vec!["telegram_send".to_string()]);
}

#[test]
fn test_parse_context_manifest_missing_or_malformed_fails_soft() {
    let empty_summary = "No manifest here";
    assert_eq!(parse_context_manifest(empty_summary), None);

    let empty_block = "```context-manifest\n```";
    assert_eq!(parse_context_manifest(empty_block), None);

    let malformed_block = "```context-manifest\nrandom gibberish with no colons\n```";
    assert_eq!(parse_context_manifest(malformed_block), None);
}

#[test]
fn test_format_context_inventory() {
    let mut active_skills = HashSet::new();
    active_skills.insert("cost-estimate".to_string());

    let mut active_tools = HashSet::new();
    active_tools.insert("telegram_send".to_string());

    let table =
        AgentService::format_context_inventory(200_000, &active_skills, &active_tools, None);
    assert!(table.contains("| `/cost-estimate` | Skill |"));
    assert!(table.contains("| `telegram_send` | Lazy Tool |"));
    assert!(table.contains("Guidance: aim to keep active skills and lazy tools <= 5% target"));
}
