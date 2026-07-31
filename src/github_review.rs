use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rq_tui_github_review::{ApiRequest, GitHubTransport};

use crate::process_control::output_with_timeout;

const GITHUB_API_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GhCliTransport;

impl GitHubTransport for GhCliTransport {
    fn execute(&self, request: ApiRequest) -> Result<serde_json::Value> {
        let input = tempfile::NamedTempFile::new().context("create GitHub API request body")?;
        serde_json::to_writer(input.as_file(), &request.body)
            .context("serialize GitHub API request body")?;
        let output = output_with_timeout(
            Command::new("gh")
                .args(["api", "--method", request.method])
                .arg(&request.path)
                .args(["--input"])
                .arg(input.path()),
            &format!("gh api {} {}", request.method, request.path),
            GITHUB_API_TIMEOUT,
        )?;
        if !output.status.success() {
            bail!(
                "GitHub API request failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        serde_json::from_slice(&output.stdout).context("GitHub API returned invalid JSON")
    }
}

#[cfg(test)]
mod tests {
    use rq_tui_github_review::{ApiRequest, GitHubTransport};
    use serde_json::json;

    use super::GhCliTransport;

    #[test]
    fn production_transport_is_available_behind_the_generic_port() {
        fn accepts_transport(_: impl GitHubTransport) {}
        accepts_transport(GhCliTransport);
        let request = ApiRequest {
            method: "POST",
            path: "/graphql".into(),
            body: json!({ "query": "query { viewer { login } }" }),
        };
        assert_eq!(request.path, "/graphql");
    }
}
