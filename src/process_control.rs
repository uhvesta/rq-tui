use std::io::Read;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_CAPTURE_BYTES: u64 = 128 * 1024 * 1024;

pub(crate) fn output_with_timeout(
    command: &mut Command,
    operation: &str,
    timeout: Duration,
) -> Result<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1");
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to start {operation}"))?;
    let stdout = child
        .stdout
        .take()
        .context("child stdout was not captured")?;
    let stderr = child
        .stderr
        .take()
        .context("child stderr was not captured")?;
    let stdout_reader = thread::spawn(move || read_bounded(stdout));
    let stderr_reader = thread::spawn(move || read_bounded(stderr));
    let started = Instant::now();

    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("failed while waiting for {operation}"))?
        {
            break status;
        }
        if started.elapsed() >= timeout {
            terminate(&mut child);
            child.wait().ok();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            bail!(
                "{operation} exceeded {} and was stopped; authentication prompts are disabled",
                format_timeout(timeout)
            );
        }
        thread::sleep(POLL_INTERVAL.min(timeout.saturating_sub(started.elapsed())));
    };

    let stdout = join_reader(stdout_reader, operation, "stdout")?;
    let stderr = join_reader(stderr_reader, operation, "stderr")?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn terminate(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // The child is its own process-group leader, so this also stops a Git
        // credential helper, SSH transport, or shell descendant holding the
        // captured pipes open.
        Command::new("/bin/kill")
            .args(["-KILL", &format!("-{}", child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok();
    }
    child.kill().ok();
}

fn read_bounded(reader: impl Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_CAPTURE_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("failed to read child output")?;
    if bytes.len() as u64 > MAX_CAPTURE_BYTES {
        bail!(
            "child output exceeded the {} MiB safety limit",
            MAX_CAPTURE_BYTES / 1024 / 1024
        );
    }
    Ok(bytes)
}

fn join_reader(
    reader: thread::JoinHandle<Result<Vec<u8>>>,
    operation: &str,
    stream: &str,
) -> Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| anyhow::anyhow!("{operation} {stream} reader panicked"))?
        .with_context(|| format!("could not capture {operation} {stream}"))
}

fn format_timeout(timeout: Duration) -> String {
    if timeout.as_secs() > 0 {
        format!("{}s", timeout.as_secs())
    } else {
        format!("{}ms", timeout.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::output_with_timeout;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn hung_process_is_stopped_at_its_deadline() {
        let started = Instant::now();
        let error = output_with_timeout(
            Command::new("sh").args(["-c", "sleep 5"]),
            "deliberately hung command",
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(error.to_string().contains("exceeded 50ms"));
    }

    #[test]
    fn child_stdin_is_closed_and_prompts_are_disabled() {
        let output = output_with_timeout(
            Command::new("sh").args([
                "-c",
                "if read value; then exit 9; fi; printf '%s:%s' \"$GIT_TERMINAL_PROMPT\" \"$GH_PROMPT_DISABLED\"",
            ]),
            "noninteractive command",
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"0:1");
    }
}
