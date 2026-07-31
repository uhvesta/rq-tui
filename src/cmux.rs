use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::process_control::output_with_timeout;

const CMUX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Debug)]
pub(crate) enum MarkdownSurface {
    Cmux {
        cli: PathBuf,
        socket: String,
        workspace_id: String,
        surface_id: String,
    },
    SystemBrowser,
}

impl MarkdownSurface {
    pub(crate) fn available() -> bool {
        cfg!(any(target_os = "macos", target_os = "linux"))
    }

    pub(crate) fn open(url: &str) -> Result<Option<Self>> {
        let Some(context) = CmuxContext::discover() else {
            let output = run(
                Command::new(platform_browser_opener()).arg(url),
                "system browser preview open",
            )?;
            ensure_success("system browser preview open", &output)?;
            return Ok(Some(Self::SystemBrowser));
        };
        let output = run(
            Command::new(&context.cli).args(open_args(
                &context.socket,
                &context.workspace_id,
                &context.surface_id,
                url,
            )),
            "cmux browser preview open",
        )?;
        let payload = serde_json::from_slice::<Value>(&output.stdout).ok();
        if !output.status.success() {
            if let Some(surface_id) = payload
                .as_ref()
                .and_then(|value| result_string(value, "surface_id"))
            {
                Self::Cmux {
                    cli: context.cli.clone(),
                    socket: context.socket.clone(),
                    workspace_id: context.workspace_id.clone(),
                    surface_id,
                }
                .close_detached()
                .ok();
            }
            ensure_success("cmux browser preview open", &output)?;
        }
        let payload = payload.context("cmux returned invalid JSON")?;
        let workspace_id =
            result_string(&payload, "workspace_id").unwrap_or_else(|| context.workspace_id.clone());
        let surface_id = result_string(&payload, "surface_id")
            .or_else(|| result_string(&payload, "panel_id"))
            .context("cmux did not return the created Markdown surface id")?;
        Ok(Some(Self::Cmux {
            cli: context.cli,
            socket: context.socket,
            workspace_id,
            surface_id,
        }))
    }

    pub(crate) fn close(self) -> Result<()> {
        match self {
            Self::Cmux {
                cli,
                socket,
                workspace_id,
                surface_id,
            } => {
                let output = run(
                    Command::new(cli).args([
                        "--socket",
                        &socket,
                        "close-surface",
                        "--workspace",
                        &workspace_id,
                        "--surface",
                        &surface_id,
                    ]),
                    "cmux close-surface",
                )?;
                ensure_success("cmux close-surface", &output)
            }
            Self::SystemBrowser => Ok(()),
        }
    }

    pub(crate) fn close_detached(self) -> Result<()> {
        let Self::Cmux {
            cli,
            socket,
            workspace_id,
            surface_id,
        } = self
        else {
            return Ok(());
        };
        Command::new(cli)
            .args([
                "--socket",
                &socket,
                "close-surface",
                "--workspace",
                &workspace_id,
                "--surface",
                &surface_id,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("cannot launch cmux surface cleanup")?;
        Ok(())
    }
}

fn platform_browser_opener() -> PathBuf {
    std::env::var_os("REV_BROWSER_OPENER")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            })
        })
}

struct CmuxContext {
    cli: PathBuf,
    socket: String,
    workspace_id: String,
    surface_id: String,
}

impl CmuxContext {
    fn discover() -> Option<Self> {
        let socket = nonempty_env("CMUX_SOCKET_PATH")?;
        let workspace_id = nonempty_env("CMUX_WORKSPACE_ID")?;
        let surface_id = nonempty_env("CMUX_SURFACE_ID")?;
        let cli = nonempty_env("CMUX_BUNDLED_CLI_PATH")
            .map(PathBuf::from)
            .filter(|path| path.is_file())
            .unwrap_or_else(|| PathBuf::from("cmux"));
        Some(Self {
            cli,
            socket,
            workspace_id,
            surface_id,
        })
    }
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn open_args<'a>(
    socket: &'a str,
    workspace_id: &'a str,
    surface_id: &'a str,
    url: &'a str,
) -> Vec<&'a std::ffi::OsStr> {
    [
        std::ffi::OsStr::new("--socket"),
        std::ffi::OsStr::new(socket),
        std::ffi::OsStr::new("--json"),
        std::ffi::OsStr::new("--surface"),
        std::ffi::OsStr::new(surface_id),
        std::ffi::OsStr::new("browser"),
        std::ffi::OsStr::new("open-split"),
        std::ffi::OsStr::new(url),
        std::ffi::OsStr::new("--workspace"),
        std::ffi::OsStr::new(workspace_id),
        std::ffi::OsStr::new("--focus"),
        std::ffi::OsStr::new("false"),
    ]
    .into()
}

fn result_string(value: &Value, key: &str) -> Option<String> {
    value
        .get("result")
        .and_then(|result| result.get(key))
        .and_then(Value::as_str)
        .or_else(|| value.get(key).and_then(Value::as_str))
        .map(str::to_owned)
}

fn run(command: &mut Command, label: &str) -> Result<Output> {
    output_with_timeout(command, label, CMUX_TIMEOUT)
}

fn ensure_success(operation: &str, output: &Output) -> Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        bail!(
            "{operation} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{open_args, result_string};
    use std::ffi::OsStr;

    #[test]
    fn browser_preview_uses_only_cmux_supported_routing_flags() {
        let args = open_args(
            "/tmp/cmux.sock",
            "workspace:7",
            "surface:3",
            "http://127.0.0.1:8765/review/token",
        );
        let args = args
            .into_iter()
            .map(OsStr::to_string_lossy)
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "--socket",
                "/tmp/cmux.sock",
                "--json",
                "--surface",
                "surface:3",
                "browser",
                "open-split",
                "http://127.0.0.1:8765/review/token",
                "--workspace",
                "workspace:7",
                "--focus",
                "false",
            ]
        );
    }

    #[test]
    fn created_ids_come_from_the_explicit_result_not_the_caller_envelope() {
        let payload = serde_json::json!({
            "ok": true,
            "caller": {"surface_id": "surface:3"},
            "result": {"surface_id": "surface:9"}
        });
        assert_eq!(
            result_string(&payload, "surface_id").as_deref(),
            Some("surface:9")
        );
    }
}
