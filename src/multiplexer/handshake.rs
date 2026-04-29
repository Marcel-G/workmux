//! Pane handshake mechanisms for shell startup synchronization.
//!
//! Different backends use different mechanisms to ensure a shell is ready
//! before sending commands to a pane.

use anyhow::{Context, Result, anyhow};
#[cfg(unix)]
use nix::sys::stat::Mode;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, trace, warn};

use crate::cmd::Cmd;

/// Trait for pane handshake mechanisms.
///
/// A handshake ensures the shell has started in a pane before sending commands.
/// Different backends may use different mechanisms (tmux wait-for, named pipes, etc.)
pub trait PaneHandshake: Send {
    /// Returns a full shell command string that wraps the shell to signal readiness.
    /// Formatted for direct shell evaluation (e.g., `sh -c "..."`).
    /// Used by backends that need a single command string (tmux).
    fn wrapper_command(&self, shell: &str) -> String;

    /// Returns the raw POSIX script body that signals readiness and exec's the shell.
    /// Does NOT include `sh -c` wrapping -- the backend decides how to invoke it.
    /// Used by the shared `setup_panes` implementation, where each backend wraps
    /// the script appropriately for its CLI.
    fn script_content(&self, shell: &str) -> String {
        // Default: delegate to wrapper_command (backwards compat)
        self.wrapper_command(shell)
    }

    /// Waits for the handshake signal, consuming the handshake object.
    fn wait(self: Box<Self>) -> Result<()>;
}

/// Timeout for waiting for pane readiness (seconds)
const HANDSHAKE_TIMEOUT_SECS: u64 = 5;

/// Timeout for waiting for shell prompt after handshake (seconds).
///
/// More generous than the handshake timeout because shell initialization
/// can be slow (direnv, devenv, nvm, conda activate, etc.). The handshake
/// confirms the shell process exists; this confirms it's interactive.
const PROMPT_WAIT_TIMEOUT_SECS: u64 = 10;

/// Poll interval when waiting for shell prompt (milliseconds).
const PROMPT_POLL_INTERVAL_MS: u64 = 50;

/// Manages the tmux wait-for handshake protocol for pane synchronization.
///
/// This struct encapsulates the channel-based handshake mechanism that ensures
/// the shell is ready before sending commands. The handshake uses tmux's `wait-for`
/// feature with channel locking to synchronize between the process spawning the
/// pane and the shell that starts inside it.
///
/// # Protocol
/// 1. Lock a unique channel (on construction)
/// 2. Start the shell with a wrapper that unlocks the channel when ready
/// 3. Wait for the shell to signal readiness (wait blocks until unlock)
/// 4. Clean up the channel
pub struct TmuxHandshake {
    channel: String,
}

impl TmuxHandshake {
    /// Create a new handshake and lock the channel.
    ///
    /// The channel must be locked before spawning the pane to ensure we don't
    /// miss the signal even if the shell starts instantly.
    pub fn new() -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();
        let channel = format!("wm_ready_{}_{}", pid, nanos);

        // Lock the channel (ensures we don't miss the signal)
        Cmd::new("tmux")
            .args(&["wait-for", "-L", &channel])
            .run()
            .context("Failed to initialize wait channel")?;

        Ok(Self { channel })
    }
}

impl PaneHandshake for TmuxHandshake {
    /// Build a shell wrapper command that signals readiness.
    ///
    /// The wrapper briefly disables echo while signaling the channel, restores it,
    /// then exec's into the shell so the TTY starts in a normal state.
    ///
    /// We wrap in `sh -c "..."` with double quotes to ensure the command works when
    /// tmux's default-shell is a non-POSIX shell like nushell. Single-quote escaping
    /// (`'\''`) doesn't work reliably when nushell parses the command before passing
    /// it to sh.
    fn wrapper_command(&self, shell: &str) -> String {
        let escaped_shell = super::util::escape_for_sh_c_inner_single_quote(shell);
        format!(
            "sh -c \"stty -echo 2>/dev/null; tmux wait-for -U {}; stty echo 2>/dev/null; exec '{}' -l\"",
            self.channel, escaped_shell
        )
    }

    fn script_content(&self, shell: &str) -> String {
        format!(
            "stty -echo 2>/dev/null; tmux wait-for -U {}; stty echo 2>/dev/null; exec '{}' -l",
            self.channel, shell
        )
    }

    /// Wait for the shell to signal it is ready, then clean up.
    ///
    /// This method consumes the handshake to ensure cleanup happens exactly once.
    /// Uses a polling loop with timeout to prevent indefinite hangs if the pane
    /// fails to start.
    fn wait(self: Box<Self>) -> Result<()> {
        debug!(channel = %self.channel, "tmux:handshake start");

        let mut child = std::process::Command::new("tmux")
            .args(["wait-for", "-L", &self.channel])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("Failed to spawn tmux wait-for command")?;

        let start = Instant::now();
        let timeout = Duration::from_secs(HANDSHAKE_TIMEOUT_SECS);

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    if status.success() {
                        // Cleanup: unlock the channel we just re-locked
                        Cmd::new("tmux")
                            .args(&["wait-for", "-U", &self.channel])
                            .run()
                            .context("Failed to cleanup wait channel")?;
                        debug!(channel = %self.channel, "tmux:handshake success");
                        return Ok(());
                    } else {
                        // Attempt cleanup even on failure
                        let _ = Cmd::new("tmux")
                            .args(&["wait-for", "-U", &self.channel])
                            .run();
                        warn!(channel = %self.channel, status = ?status.code(), "tmux:handshake failed (wait-for error)");
                        return Err(anyhow!(
                            "Pane handshake failed - tmux wait-for returned error"
                        ));
                    }
                }
                Ok(None) => {
                    if start.elapsed() >= timeout {
                        let _ = child.kill();
                        let _ = child.wait(); // Ensure process is reaped

                        // Attempt cleanup
                        let _ = Cmd::new("tmux")
                            .args(&["wait-for", "-U", &self.channel])
                            .run();

                        warn!(
                            channel = %self.channel,
                            timeout_secs = HANDSHAKE_TIMEOUT_SECS,
                            "tmux:handshake timeout"
                        );
                        return Err(anyhow!(
                            "Pane handshake timed out after {}s - shell may have failed to start",
                            HANDSHAKE_TIMEOUT_SECS
                        ));
                    }
                    trace!(
                        channel = %self.channel,
                        elapsed_ms = start.elapsed().as_millis(),
                        "tmux:handshake waiting"
                    );
                    thread::sleep(Duration::from_millis(50));
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = Cmd::new("tmux")
                        .args(&["wait-for", "-U", &self.channel])
                        .run();
                    warn!(channel = %self.channel, error = %e, "tmux:handshake error");
                    return Err(anyhow!("Error waiting for pane handshake: {}", e));
                }
            }
        }
    }
}

/// Unix named pipe (FIFO) based handshake for backends without wait-for.
///
/// Used by WezTerm and other backends that don't have a built-in synchronization
/// mechanism like tmux's wait-for.
#[cfg(unix)]
pub struct UnixPipeHandshake {
    pipe_path: PathBuf,
}

#[cfg(unix)]
impl UnixPipeHandshake {
    /// Create a new pipe handshake.
    ///
    /// Creates a named pipe (FIFO) that the shell will write to when ready.
    pub fn new() -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let pid = std::process::id();

        let pipe_path = std::env::temp_dir().join(format!("workmux_pipe_{}_{}", pid, nanos));

        // Create FIFO with 0o600 permissions (owner read/write only)
        let mode = Mode::S_IRUSR | Mode::S_IWUSR;
        nix::unistd::mkfifo(&pipe_path, mode).context("Failed to create named pipe")?;

        Ok(Self { pipe_path })
    }
}

#[cfg(unix)]
impl PaneHandshake for UnixPipeHandshake {
    fn wrapper_command(&self, shell: &str) -> String {
        let escaped_shell = super::util::escape_for_sh_c_inner_single_quote(shell);
        format!(
            "sh -c 'echo ready > {}; exec '\\''{}'\\'' -l'",
            self.pipe_path.display(),
            escaped_shell
        )
    }

    fn script_content(&self, shell: &str) -> String {
        format!(
            "echo ready > {}; exec '{}' -l",
            self.pipe_path.display(),
            shell
        )
    }

    fn wait(self: Box<Self>) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::io::AsRawFd;

        const POLL_INTERVAL_MS: u64 = 50;

        // Open pipe for reading (non-blocking)
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&self.pipe_path)
            .context("Failed to open pipe for reading")?;

        let fd = file.as_raw_fd();
        let start = Instant::now();
        let timeout = Duration::from_secs(HANDSHAKE_TIMEOUT_SECS);

        loop {
            // Check if data available via poll()
            let mut pollfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };

            let poll_timeout_ms = POLL_INTERVAL_MS as i32;
            let ret = unsafe { libc::poll(&mut pollfd, 1, poll_timeout_ms) };

            if ret > 0 && (pollfd.revents & libc::POLLIN) != 0 {
                // Data available - read and verify we got data
                let mut buf = [0u8; 64];
                let bytes_read =
                    unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                let _ = std::fs::remove_file(&self.pipe_path);

                if bytes_read > 0 {
                    return Ok(());
                } else {
                    // EOF (0) or error (-1) - writer closed without sending data
                    return Err(anyhow!("Pipe closed without receiving handshake signal"));
                }
            }

            if start.elapsed() >= timeout {
                let _ = std::fs::remove_file(&self.pipe_path);
                return Err(anyhow!(
                    "Pane handshake timed out after {}s - shell may have failed to start",
                    HANDSHAKE_TIMEOUT_SECS
                ));
            }

            // Continue polling
        }
    }
}

#[cfg(unix)]
impl Drop for UnixPipeHandshake {
    fn drop(&mut self) {
        // Clean up the pipe file if it still exists
        let _ = std::fs::remove_file(&self.pipe_path);
    }
}

/// Wait for a shell prompt to appear in the pane, indicating the shell is
/// fully interactive and ready to receive commands.
///
/// The existing handshake signals readiness *before* `exec <shell> -l`, so
/// `send_keys` can arrive while the shell is still initializing. This function
/// bridges that gap by polling `capture_pane` until the prompt is visible.
///
/// Best-effort: if the timeout expires (e.g. unusual prompt that doesn't match
/// our heuristic), we log a warning and return `Ok(())` so `send_keys` proceeds
/// anyway. This preserves backwards compatibility for exotic setups.
pub fn wait_for_prompt<M: super::Multiplexer + ?Sized>(mux: &M, pane_id: &str) -> Result<()> {
    let start = Instant::now();
    let timeout = Duration::from_secs(PROMPT_WAIT_TIMEOUT_SECS);
    let poll_interval = Duration::from_millis(PROMPT_POLL_INTERVAL_MS);

    debug!(pane_id = %pane_id, "waiting for shell prompt");

    loop {
        if let Some(content) = mux.capture_pane(pane_id, 5) {
            if has_shell_prompt(&content) {
                debug!(
                    pane_id = %pane_id,
                    elapsed_ms = start.elapsed().as_millis(),
                    "shell prompt detected"
                );
                return Ok(());
            }
        }

        if start.elapsed() >= timeout {
            warn!(
                pane_id = %pane_id,
                timeout_secs = PROMPT_WAIT_TIMEOUT_SECS,
                "timed out waiting for shell prompt, proceeding anyway"
            );
            return Ok(());
        }

        thread::sleep(poll_interval);
    }
}

/// Check if captured pane content contains a shell prompt on the last non-empty line.
///
/// Strips ANSI escape codes first, then checks whether the last non-blank line
/// ends with a character commonly used as a prompt suffix.
fn has_shell_prompt(content: &str) -> bool {
    let stripped = console::strip_ansi_codes(content);

    let last_line = stripped.lines().rev().find(|line| !line.trim().is_empty());

    let Some(line) = last_line else {
        return false;
    };

    let trimmed = line.trim_end();
    if trimmed.is_empty() {
        return false;
    }

    // Common prompt-ending characters:
    // $  - bash default, zsh with some themes
    // %  - zsh default
    // #  - root shells
    // >  - nushell, powershell, some custom prompts
    // ❯  - starship, pure, powerlevel10k
    // ➜  - oh-my-zsh robbyrussell theme
    // ›  - some custom prompts
    matches!(
        trimmed.chars().last(),
        Some('$' | '%' | '#' | '>' | '❯' | '➜' | '›')
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_bash_dollar_prompt() {
        assert!(has_shell_prompt("user@host:~$"));
        assert!(has_shell_prompt("user@host:~/dev $"));
    }

    #[test]
    fn detects_zsh_percent_prompt() {
        assert!(has_shell_prompt("user@host %"));
        assert!(has_shell_prompt("%"));
    }

    #[test]
    fn detects_root_hash_prompt() {
        assert!(has_shell_prompt("root@host:~#"));
        assert!(has_shell_prompt("# "));
    }

    #[test]
    fn detects_starship_prompt() {
        assert!(has_shell_prompt("~/dev ❯"));
        assert!(has_shell_prompt("❯"));
    }

    #[test]
    fn detects_ohmyzsh_arrow_prompt() {
        // robbyrussell theme puts ➜ at the start, not the end.
        // The full line is like "➜  workmux git:(main) ✗" — we won't detect
        // that, but the prompt cursor appears after a trailing space which we
        // can't distinguish. This is an accepted limitation; the timeout
        // fallback handles these cases gracefully.
        assert!(has_shell_prompt("➜"));
    }

    #[test]
    fn detects_nushell_prompt() {
        assert!(has_shell_prompt("~>"));
        assert!(has_shell_prompt("/home/user >"));
    }

    #[test]
    fn detects_prompt_with_ansi_codes() {
        // Simulated colored prompt: \x1b[32muser@host\x1b[0m $
        assert!(has_shell_prompt("\x1b[32muser@host\x1b[0m $"));
        assert!(has_shell_prompt("\x1b[1;34m~/dev\x1b[0m \x1b[33m❯\x1b[0m"));
    }

    #[test]
    fn skips_blank_trailing_lines() {
        assert!(has_shell_prompt("user@host:~$\n\n\n"));
        assert!(has_shell_prompt("some output\nuser@host %\n  \n"));
    }

    #[test]
    fn rejects_empty_content() {
        assert!(!has_shell_prompt(""));
        assert!(!has_shell_prompt("\n\n\n"));
        assert!(!has_shell_prompt("   \n   "));
    }

    #[test]
    fn rejects_content_without_prompt() {
        assert!(!has_shell_prompt("Loading direnv..."));
        assert!(!has_shell_prompt("Welcome to Ubuntu 22.04"));
        assert!(!has_shell_prompt("Last login: Mon Apr 28"));
    }
}
