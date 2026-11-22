use crate::brain::tools::classify::{EffectClass, classify_call};
use serde_json::json;

#[test]
fn bash_reads_are_read_only_and_writes_are_mutating() {
    assert_eq!(
        classify_call("bash", &json!({"command": "ls -la"})),
        EffectClass::ReadOnly
    );
    assert_eq!(
        classify_call("bash", &json!({"command": "printf x > out.txt"})),
        EffectClass::Mutating
    );
    assert_eq!(
        classify_call("bash", &json!({"command": "git push origin main"})),
        EffectClass::Destructive
    );
}

#[test]
fn oc_commands_distinguish_safe_and_destructive_operations() {
    assert_eq!(
        classify_call("oc-issue-list", &json!({"state": "open"})),
        EffectClass::ReadOnly
    );
    assert_eq!(
        classify_call("oc-ship-chain", &json!({"sha": "abc"})),
        EffectClass::Destructive
    );
}

#[test]
fn ordinary_file_tools_have_static_classes() {
    assert_eq!(
        classify_call("read_file", &json!({"path": "README.md"})),
        EffectClass::ReadOnly
    );
    assert_eq!(
        classify_call("write_file", &json!({"path": "README.md", "content": "x"})),
        EffectClass::Mutating
    );
}
