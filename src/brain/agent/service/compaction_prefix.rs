//! Canonical compaction-window prefix fingerprint (#1023 idle eager apply, #bug-B).
//!
//! Idle spawn and apply must hash the same DB-backed window after
//! `clean_rows_for_llm`, with content normalized so tier-0 offload stubs in
//! memory match full tool bodies still in persisted ledgers until repaired.

use super::compaction_offload::{OFFLOAD_MIN_RESULT_BYTES, is_offload_stub_for_fingerprint};
use crate::db::models::Message as DbMessage;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use uuid::Uuid;

const FINGERPRINT_STUB_TOKEN: &str = "[[TIER0_OFFLOAD_STUB]]";

/// Hash the first `snapshot_len` persisted rows the compaction loader would see.
pub(crate) fn compaction_prefix_fingerprint_db_rows(
    rows: &[DbMessage],
    snapshot_len: usize,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    let end = snapshot_len.min(rows.len());
    for row in &rows[..end] {
        row.id.hash(&mut hasher);
        row.sequence.hash(&mut hasher);
        row.role.hash(&mut hasher);
        normalize_db_content_for_fingerprint(&row.content).hash(&mut hasher);
        if let Some(thinking) = row.thinking.as_deref() {
            normalize_db_content_for_fingerprint(thinking).hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn normalize_db_content_for_fingerprint(content: &str) -> String {
    if is_offload_stub_for_fingerprint(content) {
        return FINGERPRINT_STUB_TOKEN.to_string();
    }
    if content.contains("<!-- tools-v2:") {
        return normalize_tools_v2_markers_in_content(content);
    }
    if content.len() >= OFFLOAD_MIN_RESULT_BYTES {
        return FINGERPRINT_STUB_TOKEN.to_string();
    }
    content.to_string()
}

fn normalize_tools_v2_markers_in_content(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find("<!-- tools-v2:") {
        out.push_str(&rest[..start]);
        let after_prefix = &rest[start + "<!-- tools-v2:".len()..];
        let array_start = match after_prefix.find('[') {
            Some(i) => i,
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        };
        let scan = &after_prefix[array_start..];
        let Some(array_end_rel) = find_balanced_json_end(scan) else {
            out.push_str(&rest[start..]);
            return out;
        };
        let json_slice = &scan[..=array_end_rel];
        let normalized_json = normalize_tools_v2_json_array(json_slice);
        out.push_str("<!-- tools-v2: ");
        out.push_str(&normalized_json);
        let tail = &scan[array_end_rel + 1..];
        if let Some(closer) = tail.find("-->") {
            out.push_str(&tail[..closer + 3]);
            rest = &tail[closer + 3..];
        } else {
            out.push_str(tail);
            break;
        }
    }
    out.push_str(rest);
    out
}

fn normalize_tools_v2_json_array(json_slice: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(json_slice) else {
        return json_slice.to_string();
    };
    if let Some(arr) = value.as_array_mut() {
        for entry in arr.iter_mut() {
            let Some(o) = entry.get("o").and_then(|v| v.as_str()) else {
                continue;
            };
            let normalized =
                if is_offload_stub_for_fingerprint(o) || o.len() >= OFFLOAD_MIN_RESULT_BYTES {
                    FINGERPRINT_STUB_TOKEN
                } else {
                    o
                };
            if normalized != o {
                entry["o"] = serde_json::Value::String(normalized.to_string());
            }
        }
    }
    serde_json::to_string(&value).unwrap_or_else(|_| json_slice.to_string())
}

fn find_balanced_json_end(scan: &str) -> Option<usize> {
    let bytes = scan.as_bytes();
    if bytes.first() != Some(&b'[') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                depth -= 1;
                if depth == 0 && b == b']' {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Replace a tool output inside persisted `<!-- tools-v2: ... -->` ledgers.
pub(crate) fn patch_tools_v2_output_in_content(
    content: &str,
    body_before_stub: &str,
    stub: &str,
) -> Option<String> {
    if !content.contains("<!-- tools-v2:") {
        return None;
    }
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    let mut patched = false;
    while let Some(start) = rest.find("<!-- tools-v2:") {
        out.push_str(&rest[..start]);
        let marker_start = &rest[start..];
        let after_prefix = &marker_start["<!-- tools-v2:".len()..];
        let array_start = after_prefix.find('[')?;
        let scan = &after_prefix[array_start..];
        let array_end_rel = find_balanced_json_end(scan)?;
        let json_slice = &scan[..=array_end_rel];
        let (new_json, did_patch) = patch_tools_v2_json_array(json_slice, body_before_stub, stub)?;
        out.push_str("<!-- tools-v2: ");
        out.push_str(&new_json);
        let tail = &scan[array_end_rel + 1..];
        let closer = tail.find("-->")?;
        out.push_str(&tail[..closer + 3]);
        patched |= did_patch;
        rest = &tail[closer + 3..];
    }
    if !patched {
        return None;
    }
    out.push_str(rest);
    Some(out)
}

fn patch_tools_v2_json_array(
    json_slice: &str,
    body_before_stub: &str,
    stub: &str,
) -> Option<(String, bool)> {
    let mut value: serde_json::Value = serde_json::from_str(json_slice).ok()?;
    let mut did_patch = false;
    if let Some(arr) = value.as_array_mut() {
        for entry in arr.iter_mut() {
            if let Some(o) = entry.get("o").and_then(|v| v.as_str())
                && o == body_before_stub
            {
                entry["o"] = serde_json::Value::String(stub.to_string());
                did_patch = true;
            }
        }
    }
    if !did_patch {
        return None;
    }
    Some((serde_json::to_string(&value).ok()?, true))
}

/// Persist tier-0 offload stub onto rows that still hold the full tool body (#bug-A).
pub(crate) async fn persist_tool_offload_stub(
    session_id: Uuid,
    tool_use_id: &str,
    body_before_stub: &str,
    stub: &str,
    message_service: &crate::services::MessageService,
) {
    let rows = match message_service.list_messages_for_session(session_id).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("Tier-0 offload: failed to list messages for persist: {e}");
            return;
        }
    };

    for row in rows.iter().rev() {
        if row.role == "assistant"
            && let Some(patched) =
                patch_tools_v2_output_in_content(&row.content, body_before_stub, stub)
        {
            if let Err(e) = message_service.set_content(row.id, &patched).await {
                tracing::warn!(
                    "Tier-0 offload: failed to patch assistant ledger for {tool_use_id}: {e}"
                );
            } else {
                tracing::debug!(
                    "Tier-0 offload: persisted stub on assistant row {} ({tool_use_id})",
                    row.id
                );
                return;
            }
        }
    }

    for row in rows.iter().rev() {
        if row.role == "user" && row.content == body_before_stub {
            if let Err(e) = message_service.set_content(row.id, stub).await {
                tracing::warn!(
                    "Tier-0 offload: failed to patch user tool-result row for {tool_use_id}: {e}"
                );
            } else {
                tracing::debug!(
                    "Tier-0 offload: persisted stub on user row {} ({tool_use_id})",
                    row.id
                );
                return;
            }
        }
    }

    tracing::warn!(
        "Tier-0 offload: no DB row matched tool_use_id={tool_use_id} for persist (session {session_id})"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::Message as DbMessage;
    use chrono::Utc;

    fn row(content: &str) -> DbMessage {
        DbMessage {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            role: "assistant".to_string(),
            content: content.to_string(),
            sequence: 1,
            created_at: Utc::now(),
            token_count: None,
            cost: None,
            input_tokens: None,
            cache_creation_tokens: None,
            cache_read_tokens: None,
            thinking: None,
            duration_secs: None,
        }
    }

    #[test]
    fn fingerprint_treats_full_body_and_stub_as_equal() {
        let body = "x".repeat(OFFLOAD_MIN_RESULT_BYTES + 10);
        let stub = "[Tool output offloaded: 9000 bytes (10 lines). File: /tmp/x. Use read_file on this path to inspect.]";
        let ledger = format!(
            "answer\n<!-- tools-v2: {} -->\n",
            serde_json::to_string(&[serde_json::json!({"d":"run","s":true,"o": body})]).unwrap()
        );
        let stub_ledger = format!(
            "answer\n<!-- tools-v2: {} -->\n",
            serde_json::to_string(&[serde_json::json!({"d":"run","s":true,"o": stub})]).unwrap()
        );
        let h1 = compaction_prefix_fingerprint_db_rows(&[row(&ledger)], 1);
        let h2 = compaction_prefix_fingerprint_db_rows(&[row(&stub_ledger)], 1);
        assert_eq!(h1, h2);
    }

    #[test]
    fn patch_tools_v2_replaces_exact_output() {
        let body = "hello world output";
        let stub = "stub";
        let content = format!(
            "text <!-- tools-v2: {} --> tail",
            serde_json::to_string(&[serde_json::json!({"d":"x","s":true,"o": body})]).unwrap()
        );
        let patched = patch_tools_v2_output_in_content(&content, body, stub).unwrap();
        assert!(patched.contains(stub));
        assert!(!patched.contains(body));
    }
}
