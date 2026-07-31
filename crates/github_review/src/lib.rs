use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestRef {
    pub owner: String,
    pub repository: String,
    pub number: u64,
}

impl PullRequestRef {
    pub fn api_path(&self, suffix: &str) -> String {
        format!(
            "/repos/{}/{}/pulls/{}{}",
            self.owner, self.repository, self.number, suffix
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestSnapshot {
    pub node_id: String,
    pub title: String,
    pub body: String,
    pub url: String,
    pub author: String,
    pub head_sha: String,
    pub base_sha: String,
    pub updated_at: String,
    pub threads: Vec<ReviewThread>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewThread {
    pub node_id: String,
    pub path: String,
    pub line: Option<u64>,
    pub original_line: Option<u64>,
    pub side: DiffSide,
    pub is_outdated: bool,
    pub is_resolved: bool,
    pub viewer_can_reply: bool,
    pub comments: Vec<ReviewComment>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewComment {
    pub node_id: String,
    pub database_id: Option<u64>,
    pub author: String,
    pub body: String,
    pub created_at: String,
    pub url: String,
    pub reply_to: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum DiffSide {
    Left,
    Right,
}

impl DiffSide {
    fn rest_name(self) -> &'static str {
        match self {
            Self::Left => "LEFT",
            Self::Right => "RIGHT",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    Approve,
    RequestChanges,
    Comment,
}

impl ReviewDecision {
    fn api_name(self) -> &'static str {
        match self {
            Self::Approve => "APPROVE",
            Self::RequestChanges => "REQUEST_CHANGES",
            Self::Comment => "COMMENT",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftInlineComment {
    pub local_id: String,
    pub path: String,
    pub line: u64,
    pub start_line: Option<u64>,
    pub side: DiffSide,
    pub body: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewSubmission {
    pub commit_id: String,
    pub decision: ReviewDecision,
    pub body: String,
    pub comments: Vec<DraftInlineComment>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ApiRequest {
    pub method: &'static str,
    pub path: String,
    pub body: Value,
}

pub trait GitHubTransport {
    fn execute(&self, request: ApiRequest) -> Result<Value>;
}

pub struct GitHubReviewClient<T> {
    transport: T,
}

impl<T> GitHubReviewClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }
}

impl<T: GitHubTransport> GitHubReviewClient<T> {
    pub fn submit_review(
        &self,
        reference: &PullRequestRef,
        submission: &ReviewSubmission,
    ) -> Result<Value> {
        if submission.decision == ReviewDecision::RequestChanges
            && submission.body.trim().is_empty()
        {
            bail!("request-changes reviews require an overall body");
        }
        let comments = submission
            .comments
            .iter()
            .map(|comment| {
                let mut value = json!({
                    "path": comment.path,
                    "line": comment.line,
                    "side": comment.side.rest_name(),
                    "body": comment.body,
                });
                if let Some(start_line) = comment.start_line {
                    value["start_line"] = json!(start_line);
                    value["start_side"] = json!(comment.side.rest_name());
                }
                value
            })
            .collect::<Vec<_>>();
        self.transport.execute(ApiRequest {
            method: "POST",
            path: reference.api_path("/reviews"),
            body: json!({
                "commit_id": submission.commit_id,
                "event": submission.decision.api_name(),
                "body": submission.body,
                "comments": comments,
            }),
        })
    }

    pub fn reply(
        &self,
        reference: &PullRequestRef,
        root_comment_id: u64,
        body: &str,
    ) -> Result<Value> {
        if body.trim().is_empty() {
            bail!("reply body cannot be empty");
        }
        self.transport.execute(ApiRequest {
            method: "POST",
            path: reference.api_path(&format!("/comments/{root_comment_id}/replies")),
            body: json!({ "body": body }),
        })
    }

    pub fn parse_snapshot(value: Value) -> Result<PullRequestSnapshot> {
        serde_json::from_value(value).context("invalid GitHub pull-request snapshot")
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[derive(Default)]
    struct FakeTransport {
        requests: RefCell<Vec<ApiRequest>>,
    }

    impl GitHubTransport for FakeTransport {
        fn execute(&self, request: ApiRequest) -> Result<Value> {
            self.requests.borrow_mut().push(request);
            Ok(json!({ "id": 42 }))
        }
    }

    fn reference() -> PullRequestRef {
        PullRequestRef {
            owner: "acme".into(),
            repository: "api".into(),
            number: 17,
        }
    }

    #[test]
    fn one_review_batches_inline_comments_with_a_decision_and_body() {
        let client = GitHubReviewClient::new(FakeTransport::default());
        client
            .submit_review(
                &reference(),
                &ReviewSubmission {
                    commit_id: "abc123".into(),
                    decision: ReviewDecision::Approve,
                    body: "Looks good after the local review.".into(),
                    comments: vec![DraftInlineComment {
                        local_id: "local-1".into(),
                        path: "src/lib.rs".into(),
                        line: 12,
                        start_line: Some(10),
                        side: DiffSide::Right,
                        body: "Please keep this invariant documented.".into(),
                    }],
                },
            )
            .unwrap();
        let requests = client.transport.requests.borrow();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/repos/acme/api/pulls/17/reviews");
        assert_eq!(requests[0].body["event"], "APPROVE");
        assert_eq!(requests[0].body["comments"][0]["start_side"], "RIGHT");
    }

    #[test]
    fn replies_target_the_existing_root_comment() {
        let client = GitHubReviewClient::new(FakeTransport::default());
        client
            .reply(&reference(), 99, "I checked this against the updated code.")
            .unwrap();
        let requests = client.transport.requests.borrow();
        assert_eq!(
            requests[0].path,
            "/repos/acme/api/pulls/17/comments/99/replies"
        );
    }

    #[test]
    fn snapshot_keeps_authors_description_and_outdated_threads() {
        let snapshot = GitHubReviewClient::<FakeTransport>::parse_snapshot(json!({
            "node_id": "PR_node",
            "title": "Improve review UX",
            "body": "## Why\nFaster reviews.",
            "url": "https://github.com/acme/api/pull/17",
            "author": "octocat",
            "head_sha": "head",
            "base_sha": "base",
            "updated_at": "2026-07-31T00:00:00Z",
            "threads": [{
                "node_id": "thread-1",
                "path": "src/lib.rs",
                "line": null,
                "original_line": 12,
                "side": "RIGHT",
                "is_outdated": true,
                "is_resolved": false,
                "viewer_can_reply": true,
                "comments": [{
                    "node_id": "comment-1",
                    "database_id": 99,
                    "author": "reviewer",
                    "body": "Can this be simpler?",
                    "created_at": "2026-07-30T00:00:00Z",
                    "url": "https://github.com/acme/api/pull/17#discussion_r99",
                    "reply_to": null
                }]
            }]
        }))
        .unwrap();
        assert_eq!(snapshot.body, "## Why\nFaster reviews.");
        assert!(snapshot.threads[0].is_outdated);
        assert_eq!(snapshot.threads[0].comments[0].author, "reviewer");
    }

    #[test]
    fn request_changes_requires_an_overall_explanation() {
        let client = GitHubReviewClient::new(FakeTransport::default());
        let error = client
            .submit_review(
                &reference(),
                &ReviewSubmission {
                    commit_id: "abc123".into(),
                    decision: ReviewDecision::RequestChanges,
                    body: " ".into(),
                    comments: vec![],
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("require an overall body"));
        assert!(client.transport.requests.borrow().is_empty());
    }
}
