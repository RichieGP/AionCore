use aionui_common::{AppError, CommandSpec, ErrorChain};
use aionui_runtime::Builder as CmdBuilder;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio::sync::{Mutex, broadcast, watch};
use tracing::{debug, error, info, warn};

use super::{CliAgentProcess, EVENT_CHANNEL_CAPACITY, STDERR_BUFFER_MAX};

impl CliAgentProcess {
    /// Spawn a new CLI subprocess in **SDK mode**.
    ///
    /// Unlike [`spawn`](Self::spawn), this does NOT start a stdout reader task.
    /// Instead, the raw stdin/stdout handles are available via [`take_stdio`](Self::take_stdio)
    /// for the ACP SDK transport to own.
    ///
    /// `data_dir` is the backend's `AppConfig.data_dir` — used as the root
    /// for child-process bun cache / tmp directories so they honour the
    /// operator's `--data-dir` choice instead of falling back to the OS
    /// local data dir.
    ///
    /// `binary_name` and `agent_id` are used to expand placeholder variables
    /// like `${AGENT_PREFIX}` in the command and environment.
    ///
    /// Background tasks are still spawned for:
    /// - stderr buffering
    /// - Process exit monitoring
    pub async fn spawn_for_sdk(
        config: CommandSpec,
        data_dir: &Path,
        binary_name: &str,
        agent_id: &str,
    ) -> Result<Self, AppError> {
        let mut cmd = CmdBuilder::new(&config.command);

        let placeholders = super::placeholders::placeholder_env(data_dir, binary_name, agent_id);

        // Materialise the agent's `--prefix` directory + the shared npm cache.
        // Failure here is a real environment problem, not a backend bug.
        let agent_prefix = std::path::PathBuf::from(&placeholders["AGENT_PREFIX"]);
        let npm_cache = std::path::PathBuf::from(&placeholders["AGENT_NPM_CACHE"]);
        if let Err(e) = std::fs::create_dir_all(&agent_prefix) {
            return Err(AppError::EnvironmentError(format!(
                "Failed to prepare agent runtime directory at {}: {e}",
                agent_prefix.display()
            )));
        }
        if let Err(e) = std::fs::create_dir_all(&npm_cache) {
            return Err(AppError::EnvironmentError(format!(
                "Failed to prepare npm cache at {}: {e}",
                npm_cache.display()
            )));
        }

        cmd.args(&config.args)
            .envs(config.env.iter().map(|e| (&e.name, &e.value)))
            .envs(Self::agent_spawn_env(data_dir))
            .expand_placeholders(&placeholders)
            .map_err(|e| AppError::Internal(format!("Unknown placeholder in agent command: {e}")))?;
        // The expand_placeholders call rebuilds the inner Command and resets
        // stdio. Re-apply the SDK-mode stdio config so the rest of this fn
        // can take the handles.
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        if let Some(ref cwd) = config.cwd {
            cmd.current_dir(cwd);
        }
        let preview = cmd.to_string();
        info!(command = %preview, "Spawning CLI process (SDK mode)");
        let mut child: Child = cmd.spawn().map_err(|e| {
            error!(command = %preview, error = %ErrorChain(&e), "Failed to spawn CLI process");
            AppError::Internal(format!("Failed to spawn CLI process '{preview}': {e}"))
        })?;

        let pid = child.id().ok_or_else(|| {
            error!(command = %preview, "Failed to obtain PID from spawned process");
            AppError::Internal("Failed to obtain PID from spawned process".into())
        })?;
        info!(pid, command = %preview, "CLI process spawned (SDK mode)");

        let stdout = child.stdout.take().ok_or_else(|| {
            error!(pid, "Failed to capture stdout from child process");
            AppError::Internal("Failed to capture stdout from child process".into())
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            error!(pid, "Failed to capture stderr from child process");
            AppError::Internal("Failed to capture stderr from child process".into())
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            error!(pid, "Failed to capture stdin for child process");
            AppError::Internal("Failed to capture stdin for child process".into())
        })?;

        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let (exit_tx, exit_rx) = watch::channel(None);

        // Background task: read stderr → ring buffer + log
        let stderr_buffer = Arc::new(Mutex::new(String::new()));
        let stderr_buf_clone = Arc::clone(&stderr_buffer);
        let stderr_handle = tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    warn!(pid, stderr = trimmed, "CLI process stderr");
                }
                let mut buf = stderr_buf_clone.lock().await;
                buf.push_str(&line);
                buf.push('\n');
                if buf.len() > STDERR_BUFFER_MAX {
                    let cut = buf.len() - STDERR_BUFFER_MAX;
                    buf.drain(..cut);
                }
            }

            debug!(pid, "Stderr reader finished");
        });

        // Background task: monitor process exit
        let exit_handle = tokio::spawn(async move {
            match child.wait().await {
                Ok(status) => {
                    info!(pid, ?status, "CLI process exited");
                    let _ = exit_tx.send(Some(status));
                }
                Err(e) => {
                    error!(pid, error = %ErrorChain(&e), "Failed to wait on CLI process");
                    let _ = exit_tx.send(None);
                }
            }
        });

        Ok(Self {
            stdin: Mutex::new(Some(stdin)),
            stdout: Mutex::new(Some(stdout)),
            pid,
            event_tx,
            exit_rx,
            initial_rx: std::sync::Mutex::new(None),
            stderr_buffer,
            _stdout_handle: None,
            _stderr_handle: Arc::new(stderr_handle),
            _exit_handle: Arc::new(exit_handle),
        })
    }

    /// Build environment variables for agent subprocess spawn.
    ///
    /// PATH enrichment (including the bundled node bin dir) is handled
    /// globally by `aionui_runtime::enhance_process_path` during startup;
    /// children inherit it automatically. The only per-spawn addition is
    /// `CLAUDE_CODE_EXECUTABLE`, which `claude-agent-sdk` reads to skip
    /// its own `which` lookup.
    fn agent_spawn_env(_data_dir: &Path) -> Vec<(String, String)> {
        let mut env = Vec::new();
        if let Some(claude_path) = Self::find_native_claude() {
            env.push(("CLAUDE_CODE_EXECUTABLE".into(), claude_path));
        }
        env
    }

    /// Find the native Claude Code binary so `claude-agent-sdk` can spawn it
    /// directly via `CLAUDE_CODE_EXECUTABLE`.
    ///
    /// Walks `PATH` in declared order. The actual binary check is delegated
    /// to `aionui_runtime::resolve_command_in`, which honours `PATHEXT` on
    /// Windows and adds the `.cmd / .ps1 / .bat` shim fallback for
    /// npm-installed CLIs.
    fn find_native_claude() -> Option<String> {
        let path_var = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            if let Some(found) = aionui_runtime::resolve_command_in("claude", &dir) {
                return Some(found.to_string_lossy().into_owned());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::simple_script_config;
    use super::*;
    use std::time::Duration;

    // ── SDK mode tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn spawn_for_sdk_take_stdio() {
        let config = simple_script_config("read line && echo \"$line\"");
        let tmp = std::env::temp_dir();
        let proc = CliAgentProcess::spawn_for_sdk(config, &tmp, "test-bin", "test-id")
            .await
            .unwrap();

        let stdio = proc.take_stdio().await;
        assert!(stdio.is_some(), "First take_stdio should succeed");

        let stdio_again = proc.take_stdio().await;
        assert!(stdio_again.is_none(), "Second take_stdio should return None");

        proc.kill(Duration::from_millis(100)).await.unwrap();
    }
}
