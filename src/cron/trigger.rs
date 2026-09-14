use std::time::Duration;
use tokio::process::Command;

/// Condition required for a trigger to fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TriggerCondition {
    /// Fires if stdout or stderr contains any non-whitespace output (default).
    #[default]
    NonEmpty,
    /// Fires if the command exits with a non-zero exit code.
    ExitNonZero,
    /// Fires whenever the trigger command finishes executing, regardless of output or exit status.
    Always,
}

impl TriggerCondition {
    pub fn parse(s: Option<&str>) -> Self {
        match s.map(|v| v.trim().to_lowercase()).as_deref() {
            Some("exit_non_zero") | Some("exitnonzero") | Some("non_zero") => Self::ExitNonZero,
            Some("always") => Self::Always,
            _ => Self::NonEmpty,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NonEmpty => "non_empty",
            Self::ExitNonZero => "exit_non_zero",
            Self::Always => "always",
        }
    }

    pub fn should_fire(&self, result: &TriggerResult) -> bool {
        match self {
            Self::NonEmpty => !result.combined_output().trim().is_empty(),
            Self::ExitNonZero => result.exit_code != 0,
            Self::Always => true,
        }
    }
}

/// Result of executing a pre-flight trigger command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl TriggerResult {
    pub fn combined_output(&self) -> String {
        if self.stderr.is_empty() {
            self.stdout.clone()
        } else if self.stdout.is_empty() {
            self.stderr.clone()
        } else {
            format!("{}\n{}", self.stdout, self.stderr)
        }
    }
}

/// Runner for pre-flight trigger shell commands.
#[derive(Debug, Clone)]
pub struct TriggerRunner {
    timeout: Duration,
}

impl Default for TriggerRunner {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
        }
    }
}

impl TriggerRunner {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    /// Run the given shell command under `/bin/sh -c` with the configured timeout.
    pub async fn run(&self, cmd: &str) -> Result<TriggerResult, String> {
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg(cmd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("Failed to spawn trigger process: {e}"))?;

        let output_res = tokio::time::timeout(self.timeout, child.wait_with_output()).await;

        match output_res {
            Ok(Ok(output)) => {
                let exit_code = output.status.code().unwrap_or(-1);
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                Ok(TriggerResult {
                    exit_code,
                    stdout,
                    stderr,
                })
            }
            Ok(Err(e)) => Err(format!("Trigger process execution error: {e}")),
            Err(_) => Err(format!(
                "Trigger command timed out after {}s",
                self.timeout.as_secs()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trigger_condition_parsing() {
        assert_eq!(TriggerCondition::parse(None), TriggerCondition::NonEmpty);
        assert_eq!(
            TriggerCondition::parse(Some("non_empty")),
            TriggerCondition::NonEmpty
        );
        assert_eq!(
            TriggerCondition::parse(Some("exit_non_zero")),
            TriggerCondition::ExitNonZero
        );
        assert_eq!(
            TriggerCondition::parse(Some("always")),
            TriggerCondition::Always
        );
    }

    #[test]
    fn test_trigger_condition_should_fire() {
        let empty_success = TriggerResult {
            exit_code: 0,
            stdout: "".into(),
            stderr: "".into(),
        };
        let with_output_success = TriggerResult {
            exit_code: 0,
            stdout: "disk 95%\n".into(),
            stderr: "".into(),
        };
        let empty_fail = TriggerResult {
            exit_code: 1,
            stdout: "".into(),
            stderr: "".into(),
        };

        // NonEmpty
        assert!(!TriggerCondition::NonEmpty.should_fire(&empty_success));
        assert!(TriggerCondition::NonEmpty.should_fire(&with_output_success));
        assert!(!TriggerCondition::NonEmpty.should_fire(&empty_fail));

        // ExitNonZero
        assert!(!TriggerCondition::ExitNonZero.should_fire(&empty_success));
        assert!(!TriggerCondition::ExitNonZero.should_fire(&with_output_success));
        assert!(TriggerCondition::ExitNonZero.should_fire(&empty_fail));

        // Always
        assert!(TriggerCondition::Always.should_fire(&empty_success));
        assert!(TriggerCondition::Always.should_fire(&with_output_success));
        assert!(TriggerCondition::Always.should_fire(&empty_fail));
    }

    #[tokio::test]
    async fn test_trigger_runner_execution() {
        let runner = TriggerRunner::default();
        let res = runner.run("echo 'hello trigger'").await.unwrap();
        assert_eq!(res.exit_code, 0);
        assert_eq!(res.stdout.trim(), "hello trigger");
        assert!(TriggerCondition::NonEmpty.should_fire(&res));

        let res_fail = runner.run("echo 'err' >&2; exit 2").await.unwrap();
        assert_eq!(res_fail.exit_code, 2);
        assert_eq!(res_fail.stderr.trim(), "err");
        assert!(TriggerCondition::ExitNonZero.should_fire(&res_fail));
    }
}
