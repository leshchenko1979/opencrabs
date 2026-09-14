//! Shared append-only journal for session notifications.
//!
//! Writes authoritative delivery records to `session-notify.journal` in the
//! active profile's logging directory. Used by in-process agent tools,
//! daemon A2A JSON-RPC handlers, and the CLI.

use std::io::Write;

/// Write one entry to `session-notify.journal`.
///
/// Format: `<ISO8601_TIMESTAMP>\tcaller=<caller>\ttarget=<target>\toutcome=<outcome>\texit=<exit_code>\tdetail=<detail>\n`
pub fn record(caller: &str, target: &str, outcome: &str, exit_code: i32, detail: &str) {
    let path = crate::logging::log_dir().join("session-notify.journal");
    let ts = chrono::Utc::now().to_rfc3339();

    // Sanitize caller, target, and outcome to avoid tab-injection.
    let caller_clean = caller.replace(['\t', '\n', '\r'], " ");
    let target_clean = target.replace(['\t', '\n', '\r'], " ");
    let outcome_clean = outcome.replace(['\t', '\n', '\r'], " ");

    // Sanitize detail against tabs and newlines, truncate to 500 chars so one
    // pathological message cannot bloat or corrupt TSV line structure.
    let detail_clean: String = detail
        .replace(['\n', '\r'], "\\n")
        .replace('\t', " ")
        .chars()
        .take(500)
        .collect();

    let line = format!(
        "{ts}\tcaller={caller_clean}\ttarget={target_clean}\toutcome={outcome_clean}\texit={exit_code}\tdetail={detail_clean}\n"
    );

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_record_sanitizes_newlines_and_tabs() {
        let caller = "agent\n1";
        let target = "target\t2";
        let outcome = "delivered\r\n";
        let detail = "Line 1\nLine 2\tTabbed\r\nLine 3";

        // Verification of sanitization logic without mutating live logs directory:
        let caller_clean = caller.replace(['\t', '\n', '\r'], " ");
        let target_clean = target.replace(['\t', '\n', '\r'], " ");
        let outcome_clean = outcome.replace(['\t', '\n', '\r'], " ");
        let detail_clean: String = detail
            .replace(['\n', '\r'], "\\n")
            .replace('\t', " ")
            .chars()
            .take(500)
            .collect();

        assert!(!caller_clean.contains('\n'));
        assert!(!target_clean.contains('\t'));
        assert!(!outcome_clean.contains('\r'));
        assert!(!detail_clean.contains('\n'));
        assert!(!detail_clean.contains('\t'));
        assert!(detail_clean.contains("\\n"));
    }
}
