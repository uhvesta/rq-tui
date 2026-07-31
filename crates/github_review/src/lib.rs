use std::collections::HashSet;

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
    /// Overall PR reviews, independent of inline review threads. These are
    /// retained so callers can show review decisions and reconcile a review
    /// submission that completed remotely before the local process observed
    /// its response.
    #[serde(default)]
    pub reviews: Vec<PullRequestReview>,
    pub threads: Vec<ReviewThread>,
}

/// A submitted (or pending) top-level GitHub pull-request review.
///
/// GitHub represents the decision as `state` (for example `APPROVED`,
/// `CHANGES_REQUESTED`, or `COMMENTED`). Keeping GitHub's value verbatim
/// preserves unfamiliar future states without making an older client fail to
/// load an otherwise usable review snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestReview {
    pub node_id: String,
    pub database_id: Option<u64>,
    pub author: String,
    pub body: String,
    pub state: String,
    pub commit_sha: Option<String>,
    pub submitted_at: Option<String>,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewThread {
    pub node_id: String,
    pub path: String,
    #[serde(default)]
    pub start_line: Option<u64>,
    #[serde(default)]
    pub original_start_line: Option<u64>,
    pub line: Option<u64>,
    pub original_line: Option<u64>,
    #[serde(default)]
    pub start_side: Option<DiffSide>,
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

const PULL_REQUEST_QUERY: &str = r#"
query RevPullRequest($owner: String!, $repository: String!, $number: Int!, $threadsCursor: String) {
  repository(owner: $owner, name: $repository) {
    pullRequest(number: $number) {
      id
      title
      body
      url
      updatedAt
      author { login }
      headRefOid
      baseRefOid
      reviewThreads(first: 100, after: $threadsCursor) {
        nodes {
          id
          path
          startLine
          originalStartLine
          line
          originalLine
          startDiffSide
          diffSide
          isOutdated
          isResolved
          viewerCanReply
          comments(first: 100) {
            nodes {
              id
              databaseId
              author { login }
              body
              createdAt
              url
              replyTo { id }
            }
            pageInfo { hasNextPage endCursor }
          }
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}
"#;

const THREAD_COMMENTS_QUERY: &str = r#"
query RevThreadComments($threadId: ID!, $commentsCursor: String) {
  node(id: $threadId) {
    ... on PullRequestReviewThread {
      comments(first: 100, after: $commentsCursor) {
        nodes {
          id
          databaseId
          author { login }
          body
          createdAt
          url
          replyTo { id }
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}
"#;

const PULL_REQUEST_REVIEWS_QUERY: &str = r#"
query RevPullRequestReviews($owner: String!, $repository: String!, $number: Int!, $reviewsCursor: String) {
  repository(owner: $owner, name: $repository) {
    pullRequest(number: $number) {
      id
      headRefOid
      baseRefOid
      reviews(first: 100, after: $reviewsCursor) {
        nodes {
          id
          databaseId
          author { login }
          body
          state
          commit { oid }
          submittedAt
          url
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}
"#;

pub struct GitHubReviewClient<T> {
    transport: T,
}

impl<T> GitHubReviewClient<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }
}

impl<T: GitHubTransport> GitHubReviewClient<T> {
    /// Loads a complete pull-request snapshot through the GraphQL API.
    ///
    /// GitHub paginates review threads and each thread paginates its comments
    /// independently. The transport deliberately receives every request so a
    /// caller can provide authentication, retries, logging, or a deterministic
    /// fake without coupling this crate to `gh` or an HTTP implementation.
    pub fn load_snapshot(&self, reference: &PullRequestRef) -> Result<PullRequestSnapshot> {
        let mut thread_cursor = None;
        let mut seen_thread_cursors = HashSet::new();
        let mut snapshot: Option<PullRequestSnapshot> = None;
        let mut threads = Vec::new();

        loop {
            let response = self.transport.execute(ApiRequest {
                method: "POST",
                path: "/graphql".into(),
                body: json!({
                    "query": PULL_REQUEST_QUERY,
                    "operationName": "RevPullRequest",
                    "variables": {
                        "owner": reference.owner,
                        "repository": reference.repository,
                        "number": reference.number,
                        "threadsCursor": thread_cursor,
                    },
                }),
            })?;
            let data = graphql_data(&response)?;
            let pull_request = data
                .get("repository")
                .and_then(|repository| repository.get("pullRequest"))
                .context("GitHub GraphQL response did not contain the pull request")?;
            if pull_request.is_null() {
                bail!(
                    "GitHub GraphQL response did not contain pull request #{}",
                    reference.number
                );
            }

            let page_snapshot = parse_snapshot_metadata(pull_request)?;
            if let Some(first) = snapshot.as_ref() {
                if first.head_sha != page_snapshot.head_sha
                    || first.base_sha != page_snapshot.base_sha
                {
                    bail!("pull-request revision changed while GitHub pages were loading; refresh again");
                }
            } else {
                snapshot = Some(page_snapshot);
            }

            let review_threads = pull_request
                .get("reviewThreads")
                .context("GitHub GraphQL response did not contain reviewThreads")?;
            let nodes = review_threads
                .get("nodes")
                .and_then(Value::as_array)
                .context("GitHub GraphQL reviewThreads.nodes was not an array")?;
            for node in nodes {
                let (thread, comments_page) = parse_thread(node)?;
                let thread_id = thread.node_id.clone();
                let mut thread = thread;
                let mut comments_cursor = comments_page.end_cursor;
                let mut has_more_comments = comments_page.has_next_page;
                let mut seen_comment_cursors = HashSet::new();
                while has_more_comments {
                    let cursor = comments_cursor
                        .clone()
                        .context("GitHub returned hasNextPage without endCursor for comments")?;
                    if !seen_comment_cursors.insert(cursor.clone()) {
                        bail!("GitHub repeated a review-comment pagination cursor");
                    }
                    let response = self.transport.execute(ApiRequest {
                        method: "POST",
                        path: "/graphql".into(),
                        body: json!({
                            "query": THREAD_COMMENTS_QUERY,
                            "operationName": "RevThreadComments",
                            "variables": {
                                "threadId": thread_id,
                                "commentsCursor": cursor,
                            },
                        }),
                    })?;
                    let data = graphql_data(&response)?;
                    let comments = data
                        .get("node")
                        .and_then(|node| node.get("comments"))
                        .context("GitHub GraphQL response did not contain thread comments")?;
                    let comment_nodes = comments
                        .get("nodes")
                        .and_then(Value::as_array)
                        .context("GitHub GraphQL comments.nodes was not an array")?;
                    thread.comments.extend(
                        comment_nodes
                            .iter()
                            .map(parse_comment)
                            .collect::<Result<Vec<_>>>()?,
                    );
                    let page_info = parse_page_info(comments)?;
                    has_more_comments = page_info.has_next_page;
                    comments_cursor = page_info.end_cursor;
                }
                threads.push(thread);
            }

            let page_info = parse_page_info(review_threads)?;
            if !page_info.has_next_page {
                break;
            }
            let cursor = page_info
                .end_cursor
                .context("GitHub returned hasNextPage without endCursor for reviewThreads")?;
            if !seen_thread_cursors.insert(cursor.clone()) {
                bail!("GitHub repeated a review-thread pagination cursor");
            }
            thread_cursor = Some(cursor);
        }

        let mut snapshot = snapshot.context("GitHub returned no pull-request snapshot")?;
        snapshot.threads = threads;
        snapshot.reviews = self.load_reviews(reference, &snapshot)?;
        Ok(snapshot)
    }

    fn load_reviews(
        &self,
        reference: &PullRequestRef,
        expected_snapshot: &PullRequestSnapshot,
    ) -> Result<Vec<PullRequestReview>> {
        let mut cursor = None;
        let mut seen_cursors = HashSet::new();
        let mut seen_reviews = HashSet::new();
        let mut reviews = Vec::new();

        loop {
            let response = self.transport.execute(ApiRequest {
                method: "POST",
                path: "/graphql".into(),
                body: json!({
                    "query": PULL_REQUEST_REVIEWS_QUERY,
                    "operationName": "RevPullRequestReviews",
                    "variables": {
                        "owner": reference.owner,
                        "repository": reference.repository,
                        "number": reference.number,
                        "reviewsCursor": cursor,
                    },
                }),
            })?;
            let data = graphql_data(&response)?;
            let pull_request = data
                .get("repository")
                .and_then(|repository| repository.get("pullRequest"))
                .context("GitHub GraphQL response did not contain the pull request")?;
            if pull_request.is_null() {
                bail!(
                    "GitHub GraphQL response did not contain pull request #{}",
                    reference.number
                );
            }
            ensure_same_revision(expected_snapshot, pull_request)?;

            let review_connection = pull_request
                .get("reviews")
                .context("GitHub GraphQL response did not contain reviews")?;
            let nodes = review_connection
                .get("nodes")
                .and_then(Value::as_array)
                .context("GitHub GraphQL reviews.nodes was not an array")?;
            for node in nodes {
                let review = parse_review(node)?;
                if !seen_reviews.insert(review.node_id.clone()) {
                    bail!("GitHub returned the same pull-request review on more than one page");
                }
                reviews.push(review);
            }

            let page_info = parse_page_info(review_connection)?;
            if !page_info.has_next_page {
                break;
            }
            let next_cursor = page_info
                .end_cursor
                .context("GitHub returned hasNextPage without endCursor for reviews")?;
            if !seen_cursors.insert(next_cursor.clone()) {
                bail!("GitHub repeated a pull-request-review pagination cursor");
            }
            cursor = Some(next_cursor);
        }

        Ok(reviews)
    }

    pub fn submit_review(
        &self,
        reference: &PullRequestRef,
        submission: &ReviewSubmission,
    ) -> Result<Value> {
        if matches!(
            submission.decision,
            ReviewDecision::RequestChanges | ReviewDecision::Comment
        ) && submission.body.trim().is_empty()
        {
            bail!("comment and request-changes reviews require an overall body");
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
    ) -> Result<ReviewComment> {
        if body.trim().is_empty() {
            bail!("reply body cannot be empty");
        }
        let response = self.transport.execute(ApiRequest {
            method: "POST",
            path: reference.api_path(&format!("/comments/{root_comment_id}/replies")),
            body: json!({ "body": body }),
        })?;
        parse_rest_review_comment(&response)
    }

    pub fn parse_snapshot(value: Value) -> Result<PullRequestSnapshot> {
        serde_json::from_value(value).context("invalid GitHub pull-request snapshot")
    }
}

fn parse_rest_review_comment(value: &Value) -> Result<ReviewComment> {
    Ok(ReviewComment {
        node_id: required_string(value, "node_id")?,
        database_id: value.get("id").and_then(Value::as_u64),
        author: value
            .get("user")
            .and_then(|user| user.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        body: required_string(value, "body")?,
        created_at: required_string(value, "created_at")?,
        url: value
            .get("html_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        reply_to: None,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

fn graphql_data(response: &Value) -> Result<&Value> {
    if let Some(errors) = response.get("errors").and_then(Value::as_array) {
        if !errors.is_empty() {
            let messages = errors
                .iter()
                .filter_map(|error| error.get("message").and_then(Value::as_str))
                .collect::<Vec<_>>();
            bail!(
                "GitHub GraphQL request failed: {}",
                if messages.is_empty() {
                    "unknown GraphQL error".into()
                } else {
                    messages.join("; ")
                }
            );
        }
    }
    response
        .get("data")
        .context("GitHub GraphQL response did not contain data")
}

fn parse_snapshot_metadata(value: &Value) -> Result<PullRequestSnapshot> {
    Ok(PullRequestSnapshot {
        node_id: required_string(value, "id")?,
        title: required_string(value, "title")?,
        body: value
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        url: required_string(value, "url")?,
        author: value
            .get("author")
            .and_then(|author| author.get("login"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        head_sha: required_string(value, "headRefOid")?,
        base_sha: required_string(value, "baseRefOid")?,
        updated_at: required_string(value, "updatedAt")?,
        reviews: Vec::new(),
        threads: Vec::new(),
    })
}

fn ensure_same_revision(expected: &PullRequestSnapshot, value: &Value) -> Result<()> {
    let node_id = required_string(value, "id")?;
    let head_sha = required_string(value, "headRefOid")?;
    let base_sha = required_string(value, "baseRefOid")?;
    if node_id != expected.node_id || head_sha != expected.head_sha || base_sha != expected.base_sha
    {
        bail!("pull-request revision changed while GitHub pages were loading; refresh again");
    }
    Ok(())
}

fn parse_review(value: &Value) -> Result<PullRequestReview> {
    Ok(PullRequestReview {
        node_id: required_string(value, "id")?,
        database_id: optional_u64(value, "databaseId")?,
        author: value
            .get("author")
            .and_then(|author| author.get("login"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        body: value
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        state: required_string(value, "state")?,
        commit_sha: value
            .get("commit")
            .and_then(|commit| commit.get("oid"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        submitted_at: value
            .get("submittedAt")
            .and_then(Value::as_str)
            .map(str::to_owned),
        url: required_string(value, "url")?,
    })
}

fn parse_thread(value: &Value) -> Result<(ReviewThread, PageInfo)> {
    let comments = value
        .get("comments")
        .context("GitHub GraphQL review thread did not contain comments")?;
    let nodes = comments
        .get("nodes")
        .and_then(Value::as_array)
        .context("GitHub GraphQL thread comments.nodes was not an array")?;
    let thread = ReviewThread {
        node_id: required_string(value, "id")?,
        path: required_string(value, "path")?,
        start_line: optional_u64(value, "startLine")?,
        original_start_line: optional_u64(value, "originalStartLine")?,
        line: optional_u64(value, "line")?,
        original_line: optional_u64(value, "originalLine")?,
        start_side: parse_optional_side(value.get("startDiffSide"))?,
        side: parse_side(value.get("diffSide"))?,
        is_outdated: required_bool(value, "isOutdated")?,
        is_resolved: required_bool(value, "isResolved")?,
        viewer_can_reply: required_bool(value, "viewerCanReply")?,
        comments: nodes
            .iter()
            .map(parse_comment)
            .collect::<Result<Vec<_>>>()?,
    };
    Ok((thread, parse_page_info(comments)?))
}

fn parse_comment(value: &Value) -> Result<ReviewComment> {
    Ok(ReviewComment {
        node_id: required_string(value, "id")?,
        database_id: value.get("databaseId").and_then(Value::as_u64),
        author: value
            .get("author")
            .and_then(|author| author.get("login"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        body: required_string(value, "body")?,
        created_at: required_string(value, "createdAt")?,
        url: required_string(value, "url")?,
        reply_to: value
            .get("replyTo")
            .and_then(|reply_to| reply_to.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn parse_page_info(value: &Value) -> Result<PageInfo> {
    let page_info = value
        .get("pageInfo")
        .context("GitHub GraphQL connection did not contain pageInfo")?;
    Ok(PageInfo {
        has_next_page: required_bool(page_info, "hasNextPage")?,
        end_cursor: page_info
            .get("endCursor")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("GitHub GraphQL field {field} was missing or not a string"))
}

fn required_bool(value: &Value, field: &str) -> Result<bool> {
    value
        .get(field)
        .and_then(Value::as_bool)
        .with_context(|| format!("GitHub GraphQL field {field} was missing or not a boolean"))
}

fn optional_u64(value: &Value, field: &str) -> Result<Option<u64>> {
    let value = value.get(field).unwrap_or(&Value::Null);
    if value.is_null() {
        Ok(None)
    } else {
        value
            .as_u64()
            .map(Some)
            .with_context(|| format!("GitHub GraphQL field {field} was not an integer"))
    }
}

fn parse_side(value: Option<&Value>) -> Result<DiffSide> {
    match value.and_then(Value::as_str) {
        Some("LEFT") => Ok(DiffSide::Left),
        Some("RIGHT") => Ok(DiffSide::Right),
        Some(side) => bail!("unsupported GitHub GraphQL diff side {side}"),
        None => bail!("GitHub GraphQL thread did not contain diffSide"),
    }
}

fn parse_optional_side(value: Option<&Value>) -> Result<Option<DiffSide>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        value => parse_side(value).map(Some),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use super::*;

    #[derive(Default)]
    struct FakeTransport {
        requests: RefCell<Vec<ApiRequest>>,
        responses: RefCell<VecDeque<Value>>,
    }

    impl GitHubTransport for FakeTransport {
        fn execute(&self, request: ApiRequest) -> Result<Value> {
            self.requests.borrow_mut().push(request);
            Ok(self
                .responses
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| json!({ "id": 42 })))
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
        let client = GitHubReviewClient::new(FakeTransport {
            requests: RefCell::new(Vec::new()),
            responses: RefCell::new(VecDeque::from([json!({
                "node_id": "reply-node",
                "id": 100,
                "user": { "login": "octocat" },
                "body": "I checked this against the updated code.",
                "created_at": "2026-07-31T00:00:00Z",
                "html_url": "https://github.com/acme/api/pull/17#discussion_r100"
            })])),
        });
        let reply = client
            .reply(&reference(), 99, "I checked this against the updated code.")
            .unwrap();
        assert_eq!(reply.author, "octocat");
        assert_eq!(reply.database_id, Some(100));
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

    #[allow(clippy::too_many_arguments)]
    fn thread(
        id: &str,
        comment_nodes: Value,
        comments_has_next: bool,
        comments_cursor: Option<&str>,
        side: &str,
        outdated: bool,
        resolved: bool,
        can_reply: bool,
    ) -> Value {
        json!({
            "id": id,
            "path": "src/lib.rs",
            "line": 24,
            "originalLine": 20,
            "diffSide": side,
            "isOutdated": outdated,
            "isResolved": resolved,
            "viewerCanReply": can_reply,
            "comments": {
                "nodes": comment_nodes,
                "pageInfo": {
                    "hasNextPage": comments_has_next,
                    "endCursor": comments_cursor,
                }
            }
        })
    }

    fn comment(id: &str, author: &str, body: &str, reply_to: Option<&str>) -> Value {
        json!({
            "id": id,
            "databaseId": 700,
            "author": { "login": author },
            "body": body,
            "createdAt": "2026-07-31T01:02:03Z",
            "url": format!("https://github.com/acme/api/pull/17#discussion_r{id}"),
            "replyTo": reply_to.map(|id| json!({ "id": id })),
        })
    }

    fn pull_request_page(threads: Value, has_next: bool, end_cursor: Option<&str>) -> Value {
        json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "id": "PR_node_17",
                        "title": "Improve review UX",
                        "body": "## Description\nA paginated review.",
                        "url": "https://github.com/acme/api/pull/17",
                        "updatedAt": "2026-07-31T01:00:00Z",
                        "author": { "login": "octocat" },
                        "headRefOid": "head-sha-2",
                        "baseRefOid": "base-sha-1",
                        "reviewThreads": {
                            "nodes": threads,
                            "pageInfo": {
                                "hasNextPage": has_next,
                                "endCursor": end_cursor,
                            }
                        }
                    }
                }
            }
        })
    }

    fn comments_page(
        thread_id: &str,
        comments: Value,
        has_next: bool,
        cursor: Option<&str>,
    ) -> Value {
        json!({
            "data": {
                "node": {
                    "id": thread_id,
                    "comments": {
                        "nodes": comments,
                        "pageInfo": {
                            "hasNextPage": has_next,
                            "endCursor": cursor,
                        }
                    }
                }
            }
        })
    }

    fn review(
        id: &str,
        state: &str,
        commit_sha: Option<&str>,
        submitted_at: Option<&str>,
    ) -> Value {
        json!({
            "id": id,
            "databaseId": 800,
            "author": { "login": "reviewer" },
            "body": format!("Review body for {id}"),
            "state": state,
            "commit": commit_sha.map(|oid| json!({ "oid": oid })),
            "submittedAt": submitted_at,
            "url": format!("https://github.com/acme/api/pull/17#pullrequestreview-{id}"),
        })
    }

    fn reviews_page(reviews: Value, has_next: bool, end_cursor: Option<&str>) -> Value {
        json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "id": "PR_node_17",
                        "headRefOid": "head-sha-2",
                        "baseRefOid": "base-sha-1",
                        "reviews": {
                            "nodes": reviews,
                            "pageInfo": {
                                "hasNextPage": has_next,
                                "endCursor": end_cursor,
                            }
                        }
                    }
                }
            }
        })
    }

    #[test]
    fn loads_two_thread_pages_and_paginates_thread_comments() {
        let transport = FakeTransport {
            requests: RefCell::new(Vec::new()),
            responses: RefCell::new(VecDeque::from([
                pull_request_page(
                    json!([thread(
                        "thread-1",
                        json!([comment(
                            "comment-1",
                            "reviewer-one",
                            "Please simplify this.",
                            None
                        )]),
                        true,
                        Some("comments-cursor-1"),
                        "RIGHT",
                        true,
                        false,
                        true,
                    )]),
                    true,
                    Some("threads-cursor-1"),
                ),
                comments_page(
                    "thread-1",
                    json!([comment(
                        "comment-1-reply",
                        "octocat",
                        "Thanks, updated.",
                        Some("comment-1")
                    )]),
                    false,
                    None,
                ),
                pull_request_page(
                    json!([thread(
                        "thread-2",
                        json!([comment(
                            "comment-2",
                            "reviewer-two",
                            "This is resolved.",
                            Some("comment-root")
                        )]),
                        false,
                        None,
                        "LEFT",
                        false,
                        true,
                        false,
                    )]),
                    false,
                    None,
                ),
                reviews_page(json!([]), false, None),
            ])),
        };
        let client = GitHubReviewClient::new(transport);
        let snapshot = client.load_snapshot(&reference()).unwrap();

        assert_eq!(snapshot.node_id, "PR_node_17");
        assert_eq!(snapshot.author, "octocat");
        assert_eq!(snapshot.body, "## Description\nA paginated review.");
        assert_eq!(snapshot.head_sha, "head-sha-2");
        assert_eq!(snapshot.base_sha, "base-sha-1");
        assert!(snapshot.reviews.is_empty());
        assert_eq!(snapshot.threads.len(), 2);
        assert!(snapshot.threads[0].is_outdated);
        assert!(!snapshot.threads[0].is_resolved);
        assert!(snapshot.threads[0].viewer_can_reply);
        assert_eq!(snapshot.threads[0].comments.len(), 2);
        assert_eq!(snapshot.threads[0].comments[1].author, "octocat");
        assert_eq!(
            snapshot.threads[0].comments[1].reply_to.as_deref(),
            Some("comment-1")
        );
        assert!(!snapshot.threads[1].viewer_can_reply);
        assert!(snapshot.threads[1].is_resolved);
        assert_eq!(snapshot.threads[1].side, DiffSide::Left);

        let requests = client.transport.requests.borrow();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].path, "/graphql");
        assert_eq!(requests[0].body["operationName"], "RevPullRequest");
        let query = requests[0].body["query"].as_str().unwrap();
        for field in [
            "body",
            "author { login }",
            "headRefOid",
            "baseRefOid",
            "reviewThreads",
            "isOutdated",
            "isResolved",
            "viewerCanReply",
            "replyTo { id }",
        ] {
            assert!(query.contains(field), "query is missing {field}");
        }
        assert_eq!(requests[0].body["variables"]["threadsCursor"], Value::Null);
        assert_eq!(requests[1].body["operationName"], "RevThreadComments");
        assert_eq!(requests[1].body["variables"]["threadId"], "thread-1");
        assert_eq!(
            requests[1].body["variables"]["commentsCursor"],
            "comments-cursor-1"
        );
        assert_eq!(
            requests[2].body["variables"]["threadsCursor"],
            "threads-cursor-1"
        );
        assert_eq!(requests[3].body["operationName"], "RevPullRequestReviews");
        let review_query = requests[3].body["query"].as_str().unwrap();
        for field in [
            "reviews",
            "databaseId",
            "state",
            "commit { oid }",
            "submittedAt",
        ] {
            assert!(review_query.contains(field), "query is missing {field}");
        }
    }

    #[test]
    fn loads_all_overall_review_pages_with_metadata_for_reconciliation() {
        let transport = FakeTransport {
            requests: RefCell::new(Vec::new()),
            responses: RefCell::new(VecDeque::from([
                pull_request_page(json!([]), false, None),
                reviews_page(
                    json!([
                        review(
                            "review-1",
                            "APPROVED",
                            Some("head-sha-2"),
                            Some("2026-07-31T03:00:00Z")
                        ),
                        review("review-2", "PENDING", None, None),
                    ]),
                    true,
                    Some("reviews-page-2"),
                ),
                reviews_page(
                    json!([review(
                        "review-3",
                        "CHANGES_REQUESTED",
                        Some("head-sha-2"),
                        Some("2026-07-31T04:00:00Z")
                    )]),
                    false,
                    None,
                ),
            ])),
        };
        let client = GitHubReviewClient::new(transport);
        let snapshot = client.load_snapshot(&reference()).unwrap();

        assert_eq!(snapshot.reviews.len(), 3);
        assert_eq!(snapshot.reviews[0].database_id, Some(800));
        assert_eq!(snapshot.reviews[0].author, "reviewer");
        assert_eq!(snapshot.reviews[0].state, "APPROVED");
        assert_eq!(
            snapshot.reviews[0].commit_sha.as_deref(),
            Some("head-sha-2")
        );
        assert_eq!(
            snapshot.reviews[0].submitted_at.as_deref(),
            Some("2026-07-31T03:00:00Z")
        );
        assert!(snapshot.reviews[1].commit_sha.is_none());
        assert!(snapshot.reviews[1].submitted_at.is_none());

        let requests = client.transport.requests.borrow();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].body["variables"]["reviewsCursor"], Value::Null);
        assert_eq!(
            requests[2].body["variables"]["reviewsCursor"],
            "reviews-page-2"
        );
    }

    #[test]
    fn graphql_errors_are_returned_instead_of_being_treated_as_an_empty_snapshot() {
        let transport = FakeTransport {
            requests: RefCell::new(Vec::new()),
            responses: RefCell::new(VecDeque::from([json!({
                "errors": [{ "message": "Bad credentials" }]
            })])),
        };
        let client = GitHubReviewClient::new(transport);
        let error = client.load_snapshot(&reference()).unwrap_err();
        assert!(error.to_string().contains("Bad credentials"));
    }

    #[test]
    fn missing_pagination_cursor_is_a_hard_error() {
        let transport = FakeTransport {
            requests: RefCell::new(Vec::new()),
            responses: RefCell::new(VecDeque::from([pull_request_page(json!([]), true, None)])),
        };
        let client = GitHubReviewClient::new(transport);
        let error = client.load_snapshot(&reference()).unwrap_err();
        assert!(error.to_string().contains("hasNextPage without endCursor"));
    }

    #[test]
    fn repeated_pagination_cursors_are_rejected_instead_of_looping() {
        let transport = FakeTransport {
            requests: RefCell::new(Vec::new()),
            responses: RefCell::new(VecDeque::from([
                pull_request_page(json!([]), true, Some("same-cursor")),
                pull_request_page(json!([]), true, Some("same-cursor")),
            ])),
        };
        let error = GitHubReviewClient::new(transport)
            .load_snapshot(&reference())
            .unwrap_err();
        assert!(error.to_string().contains("repeated a review-thread"));
    }

    #[test]
    fn revision_changes_during_pagination_are_rejected() {
        let first = pull_request_page(json!([]), true, Some("next"));
        let mut second = pull_request_page(json!([]), false, None);
        second["data"]["repository"]["pullRequest"]["headRefOid"] = json!("newer-head");
        let transport = FakeTransport {
            requests: RefCell::new(Vec::new()),
            responses: RefCell::new(VecDeque::from([first, second])),
        };
        let error = GitHubReviewClient::new(transport)
            .load_snapshot(&reference())
            .unwrap_err();
        assert!(error.to_string().contains("revision changed"));
    }

    #[test]
    fn repeated_review_pagination_cursors_are_rejected_instead_of_looping() {
        let transport = FakeTransport {
            requests: RefCell::new(Vec::new()),
            responses: RefCell::new(VecDeque::from([
                pull_request_page(json!([]), false, None),
                reviews_page(json!([]), true, Some("same-review-cursor")),
                reviews_page(json!([]), true, Some("same-review-cursor")),
            ])),
        };
        let error = GitHubReviewClient::new(transport)
            .load_snapshot(&reference())
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("repeated a pull-request-review pagination cursor"));
    }

    #[test]
    fn revision_changes_while_loading_reviews_are_rejected() {
        let mut changed = reviews_page(json!([]), false, None);
        changed["data"]["repository"]["pullRequest"]["baseRefOid"] = json!("newer-base");
        let transport = FakeTransport {
            requests: RefCell::new(Vec::new()),
            responses: RefCell::new(VecDeque::from([
                pull_request_page(json!([]), false, None),
                changed,
            ])),
        };
        let error = GitHubReviewClient::new(transport)
            .load_snapshot(&reference())
            .unwrap_err();
        assert!(error.to_string().contains("revision changed"));
    }
}
