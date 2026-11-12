//! The GitHub adapter, for github.com and GitHub Enterprise Server: REST for the Pull Request
//! and its files, GraphQL for review Threads (REST lacks their resolved state).

use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use ureq::http::Response;
use ureq::{Body, RequestBuilder};
use url::Url;

use super::file_change;
use super::{
    CredentialSources, ForgeAdapter, ForgeError, HostConfig, PullRequest, PullRequestRef, RemoteAnchor, RemoteComment,
    RemoteThread, Review,
};
use crate::{Changeset, Side, Verdict};

/// Pages of up to this many items, GitHub's maximum.
const PAGE: &str = "100";
/// GitHub serves raw contents up to 100 MB.
const MAX_FILE: u64 = 100 * 1024 * 1024;

pub struct GitHub {
    host: HostConfig,
    /// The REST API's base URL, without a trailing `/`.
    api: String,
    graphql: String,
    credentials: CredentialSources,
    /// Resolved on first use; forgotten when the host refuses it, so a fresh login is picked up.
    token: Mutex<Option<String>>,
    agent: ureq::Agent,
}

impl GitHub {
    /// The adapter for `host`, whose API is at `host.api_base` or else derived: github.com's
    /// is `https://api.github.com`, an Enterprise host's `https://HOST/api/v3`.
    pub fn new(host: HostConfig, credentials: CredentialSources) -> Self {
        let github_com = host.host.eq_ignore_ascii_case("github.com");
        let api = match &host.api_base {
            Some(base) => base.trim_end_matches('/').to_owned(),
            None if github_com => "https://api.github.com".to_owned(),
            None => format!("https://{}/api/v3", host.host),
        };
        let graphql = match api.strip_suffix("/api/v3") {
            Some(root) => format!("{root}/api/graphql"),
            None => format!("{api}/graphql"),
        };
        let agent = ureq::Agent::config_builder()
            // Read 4xx/5xx bodies for GitHub's error messages.
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(120)))
            .user_agent(concat!("akshually/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        GitHub { host, api, graphql, credentials, token: Mutex::new(None), agent }
    }

    fn token(&self) -> Result<String, ForgeError> {
        let mut cached = self.token.lock().unwrap_or_else(|e| e.into_inner());
        if cached.is_none() {
            *cached = self.credentials.github_token(&self.host);
        }
        cached.clone().ok_or_else(|| ForgeError::NoCredentials { host: self.host.host.clone() })
    }

    /// `segments` appended to the REST base, each percent-encoded.
    fn endpoint(&self, segments: &[&str]) -> Url {
        let mut url = Url::parse(&self.api).expect("the API base is a URL");
        url.path_segments_mut().expect("the API base is a URL with a path").pop_if_empty().extend(segments);
        url
    }

    fn send<B>(
        &self,
        request: RequestBuilder<B>,
        accept: &str,
        send: impl FnOnce(RequestBuilder<B>) -> Result<Response<Body>, ureq::Error>,
    ) -> Result<Response<Body>, ForgeError> {
        let request = request
            .header("Authorization", format!("Bearer {}", self.token()?))
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", "2022-11-28");
        let mut response = send(request).map_err(|e| self.http(e))?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(response);
        }
        let host = self.host.host.clone();
        let body = response.body_mut().read_to_string().unwrap_or_default();
        Err(match status {
            401 => {
                *self.token.lock().unwrap_or_else(|e| e.into_inner()) = None;
                ForgeError::Unauthorized { host, status }
            }
            404 => ForgeError::NotFound(host),
            _ => {
                #[derive(Deserialize)]
                struct Message {
                    message: String,
                }
                let message = serde_json::from_str::<Message>(&body).map_or(body, |m| m.message);
                ForgeError::Api { host, status, message }
            }
        })
    }

    fn get(&self, url: &str, accept: &str) -> Result<Response<Body>, ForgeError> {
        self.send(self.agent.get(url), accept, RequestBuilder::call)
    }

    fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T, ForgeError> {
        self.get(url, "application/vnd.github+json")?.body_mut().read_json().map_err(|e| self.unexpected(e))
    }

    fn graphql<T: DeserializeOwned>(&self, query: &str, variables: serde_json::Value) -> Result<T, ForgeError> {
        #[derive(Deserialize)]
        struct Answer<T> {
            data: Option<T>,
            #[serde(default)]
            errors: Vec<GraphQlError>,
        }
        #[derive(Deserialize)]
        struct GraphQlError {
            message: String,
        }
        let body = serde_json::json!({ "query": query, "variables": variables });
        let answer: Answer<T> = self
            .send(self.agent.post(&self.graphql), "application/vnd.github+json", |r| r.send_json(body))?
            .body_mut()
            .read_json()
            .map_err(|e| self.unexpected(e))?;
        match (answer.data, answer.errors.into_iter().next()) {
            (Some(data), None) => Ok(data),
            (_, Some(error)) => {
                Err(ForgeError::Api { host: self.host.host.clone(), status: 200, message: error.message })
            }
            (None, None) => Err(self.unexpected("GraphQL answered no data")),
        }
    }

    /// A file's contents at `sha`.
    fn file(&self, pr: &PullRequestRef, path: &str, sha: &str) -> Result<Vec<u8>, ForgeError> {
        let mut url = self.endpoint(&repo_segments(pr, &["contents"]));
        url.path_segments_mut().expect("a path").extend(path.split('/'));
        url.query_pairs_mut().append_pair("ref", sha);
        self.get(url.as_str(), "application/vnd.github.raw+json")?
            .body_mut()
            .with_config()
            .limit(MAX_FILE)
            .read_to_vec()
            .map_err(|e| self.http(e))
    }

    fn http(&self, error: impl std::fmt::Display) -> ForgeError {
        ForgeError::Http { host: self.host.host.clone(), message: error.to_string() }
    }

    fn unexpected(&self, error: impl std::fmt::Display) -> ForgeError {
        ForgeError::Unexpected { host: self.host.host.clone(), message: error.to_string() }
    }
}

/// `repos/OWNER/REPO` followed by `rest`.
fn repo_segments<'a>(pr: &'a PullRequestRef, rest: &[&'a str]) -> Vec<&'a str> {
    let mut segments = vec!["repos"];
    segments.extend(pr.repo.split('/'));
    segments.extend(rest);
    segments
}

/// The `rel="next"` URL of a `Link` header.
fn next_page(response: &Response<Body>) -> Option<String> {
    let link = response.headers().get("link")?.to_str().ok()?;
    link.split(',').find_map(|part| {
        let (target, params) = part.split_once(';')?;
        params.split(';').any(|p| p.trim() == "rel=\"next\"").then(|| target.trim().trim_matches(['<', '>']).to_owned())
    })
}

impl ForgeAdapter for GitHub {
    fn recognise(&self, url: &str) -> Option<PullRequestRef> {
        let url = Url::parse(url).ok()?;
        if !matches!(url.scheme(), "https" | "http") {
            return None;
        }
        let host = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str()?),
            None => url.host_str()?.to_owned(),
        };
        if !host.eq_ignore_ascii_case(&self.host.host) {
            return None;
        }
        let segments: Vec<&str> = url.path_segments()?.collect();
        let [owner, repo, "pull", number, ..] = segments.as_slice() else { return None };
        if owner.is_empty() || repo.is_empty() {
            return None;
        }
        Some(PullRequestRef {
            host: self.host.host.clone(),
            repo: format!("{owner}/{repo}"),
            number: number.parse().ok()?,
        })
    }

    fn pull_request(&self, pr: &PullRequestRef) -> Result<PullRequest, ForgeError> {
        #[derive(Deserialize)]
        struct Pull {
            html_url: String,
            title: String,
            user: User,
            head: Ref,
            base: Ref,
        }
        #[derive(Deserialize)]
        struct User {
            login: String,
        }
        #[derive(Deserialize)]
        struct Ref {
            #[serde(rename = "ref")]
            name: String,
            sha: String,
        }
        let number = pr.number.to_string();
        let url = self.endpoint(&repo_segments(pr, &["pulls", &number]));
        let pull: Pull = self.get_json(url.as_str()).map_err(|e| match e {
            ForgeError::NotFound(_) => ForgeError::NotFound(pr.to_string()),
            e => e,
        })?;
        Ok(PullRequest {
            id: pr.clone(),
            url: pull.html_url,
            title: pull.title,
            author: pull.user.login,
            head_branch: pull.head.name,
            head_sha: pull.head.sha,
            base_sha: pull.base.sha,
        })
    }

    fn changeset(&self, pr: &PullRequest) -> Result<Changeset, ForgeError> {
        #[derive(Deserialize)]
        struct Compare {
            merge_base_commit: Commit,
        }
        #[derive(Deserialize)]
        struct Commit {
            sha: String,
        }
        #[derive(Deserialize)]
        struct File {
            filename: String,
            status: String,
            previous_filename: Option<String>,
        }
        let id = &pr.id;
        // `base.sha` is the base branch's tip; GitHub's diff is against the merge-base.
        let range = format!("{}...{}", pr.base_sha, pr.head_sha);
        let compare: Compare = self.get_json(self.endpoint(&repo_segments(id, &["compare", &range])).as_str())?;
        let base = compare.merge_base_commit.sha;

        let number = id.number.to_string();
        let mut url = self.endpoint(&repo_segments(id, &["pulls", &number, "files"]));
        url.query_pairs_mut().append_pair("per_page", PAGE);
        let mut next = Some(url.to_string());
        let mut listed = Vec::new();
        while let Some(url) = next.take() {
            // Only ever send the token to this host's API.
            if !url.starts_with(&format!("{}/", self.api)) {
                return Err(self.unexpected(format!("a page link leads off the API: {url}")));
            }
            let mut response = self.get(&url, "application/vnd.github+json")?;
            next = next_page(&response);
            let page: Vec<File> = response.body_mut().read_json().map_err(|e| self.unexpected(e))?;
            listed.extend(page);
        }

        // Like the local Source, renames are a deletion plus an addition.
        let mut files = Vec::new();
        for file in listed {
            let old = |path: &str| self.file(id, path, &base).map(Some);
            let new = |path: &str| self.file(id, path, &pr.head_sha).map(Some);
            match file.status.as_str() {
                "added" | "copied" => files.push(file_change(file.filename.clone(), None, new(&file.filename)?)),
                "removed" => files.push(file_change(file.filename.clone(), old(&file.filename)?, None)),
                "renamed" => {
                    let previous = file.previous_filename.unwrap_or_else(|| file.filename.clone());
                    files.push(file_change(previous.clone(), old(&previous)?, None));
                    files.push(file_change(file.filename.clone(), None, new(&file.filename)?));
                }
                "unchanged" => {}
                _ => files.push(file_change(file.filename.clone(), old(&file.filename)?, new(&file.filename)?)),
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Changeset { base, files })
    }

    fn threads(&self, pr: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
        let (owner, repo) = pr.id.repo.split_once('/').ok_or_else(|| self.unexpected("a repository without owner"))?;
        let mut threads = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let variables = serde_json::json!({ "owner": owner, "repo": repo, "number": pr.id.number, "after": after });
            let data: ThreadsData = self.graphql(THREADS_QUERY, variables)?;
            let page = data
                .repository
                .and_then(|r| r.pull_request)
                .ok_or_else(|| ForgeError::NotFound(pr.id.to_string()))?
                .review_threads;
            threads.extend(page.nodes.into_iter().map(GqlThread::into_remote));
            match page.page_info {
                PageInfo { has_next_page: true, end_cursor: Some(cursor) } => after = Some(cursor),
                _ => return Ok(threads),
            }
        }
    }

    fn verdicts(&self, pr: &PullRequest) -> Result<Vec<Verdict>, ForgeError> {
        #[derive(Deserialize)]
        struct User {
            login: String,
        }
        let viewer: User = self.get_json(self.endpoint(&["user"]).as_str())?;
        // Authors can't approve their own Pull Request, nor request changes on it.
        Ok(if viewer.login.eq_ignore_ascii_case(&pr.author) { vec![Verdict::Feedback] } else { Verdict::ALL.to_vec() })
    }

    /// A pending review, then each Thread in it, then the Verdict submits it. A failure on the
    /// way deletes the pending review, so nothing is half-published.
    fn publish(&self, pr: &PullRequest, review: &Review) -> Result<String, ForgeError> {
        let (owner, repo) = pr.id.repo.split_once('/').ok_or_else(|| self.unexpected("a repository without owner"))?;
        let variables = json!({ "owner": owner, "repo": repo, "number": pr.id.number });
        let data: Value = self.graphql(PULL_REQUEST_ID_QUERY, variables)?;
        let pr_id =
            data.pointer("/repository/pullRequest/id").ok_or_else(|| ForgeError::NotFound(pr.id.to_string()))?;
        let input = json!({ "pullRequestId": pr_id, "commitOID": pr.head_sha });
        let data: Value = self.graphql(START_REVIEW, json!({ "input": input }))?;
        let review_id = self.string_at(&data, "/addPullRequestReview/pullRequestReview/id")?;
        let submitted = self.fill_and_submit(&review_id, review);
        if submitted.is_err() {
            // Best effort: the error that stopped Publish is the one to report.
            let input = json!({ "pullRequestReviewId": review_id });
            let _ = self.graphql::<Value>(DELETE_REVIEW, json!({ "input": input }));
        }
        submitted
    }
}

impl GitHub {
    /// Adds `review`'s Threads to the pending review `review_id`, then submits it with the
    /// Verdict and body, returning the review's web page.
    fn fill_and_submit(&self, review_id: &str, review: &Review) -> Result<String, ForgeError> {
        for comment in &review.comments {
            let mut input = match &comment.anchor {
                RemoteAnchor::Lines { path, side, start, end } => {
                    let side = match side {
                        Side::Old => "LEFT",
                        Side::New => "RIGHT",
                    };
                    let mut input = json!({ "path": path, "subjectType": "LINE", "line": end, "side": side });
                    if start < end {
                        input["startLine"] = json!(start);
                        input["startSide"] = json!(side);
                    }
                    input
                }
                RemoteAnchor::File { path } => json!({ "path": path, "subjectType": "FILE" }),
            };
            input["pullRequestReviewId"] = json!(review_id);
            input["body"] = json!(match &comment.suggestion {
                Some(code) => with_suggestion(&comment.body, code),
                None => comment.body.clone(),
            });
            let data: Value = self.graphql(ADD_THREAD, json!({ "input": input }))?;
            self.string_at(&data, "/addPullRequestReviewThread/thread/id")?;
        }
        let event = match review.verdict {
            Verdict::Approve => "APPROVE",
            Verdict::RequestChanges => "REQUEST_CHANGES",
            // akshually's feedback is GitHub's "Comment".
            Verdict::Feedback => "COMMENT",
        };
        let input = json!({ "pullRequestReviewId": review_id, "event": event, "body": review.body });
        let data: Value = self.graphql(SUBMIT_REVIEW, json!({ "input": input }))?;
        self.string_at(&data, "/submitPullRequestReview/pullRequestReview/url")
    }

    /// The string at `pointer` in a GraphQL answer.
    fn string_at(&self, data: &Value, pointer: &str) -> Result<String, ForgeError> {
        data.pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| self.unexpected(format!("GraphQL answered no {pointer}")))
    }
}

/// `body` followed by GitHub's native suggestion of `code` for the commented lines, in a fence
/// longer than any backtick run in `code`. Empty `code` suggests deleting the lines.
fn with_suggestion(body: &str, code: &str) -> String {
    let longest = code.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat((longest + 1).max(3));
    let newline = if code.is_empty() || code.ends_with('\n') { "" } else { "\n" };
    let block = format!("{fence}suggestion\n{code}{newline}{fence}");
    if body.is_empty() { block } else { format!("{body}\n\n{block}") }
}

const PULL_REQUEST_ID_QUERY: &str = "query($owner: String!, $repo: String!, $number: Int!) {
  repository(owner: $owner, name: $repo) { pullRequest(number: $number) { id } }
}";

const START_REVIEW: &str = "mutation($input: AddPullRequestReviewInput!) {
  addPullRequestReview(input: $input) { pullRequestReview { id } }
}";

const ADD_THREAD: &str = "mutation($input: AddPullRequestReviewThreadInput!) {
  addPullRequestReviewThread(input: $input) { thread { id } }
}";

const SUBMIT_REVIEW: &str = "mutation($input: SubmitPullRequestReviewInput!) {
  submitPullRequestReview(input: $input) { pullRequestReview { url } }
}";

const DELETE_REVIEW: &str = "mutation($input: DeletePullRequestReviewInput!) {
  deletePullRequestReview(input: $input) { pullRequestReview { id } }
}";

const THREADS_QUERY: &str = "query($owner: String!, $repo: String!, $number: Int!, $after: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $after) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id isResolved resolvedBy { login } isOutdated path subjectType
          line originalLine startLine originalStartLine diffSide
          comments(first: 100) { nodes { body author { login } diffHunk } }
        }
      }
    }
  }
}";

#[derive(Deserialize)]
struct ThreadsData {
    repository: Option<GqlRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlRepository {
    pull_request: Option<GqlPullRequest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlPullRequest {
    review_threads: Page<GqlThread>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Page<T> {
    page_info: PageInfo,
    nodes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlThread {
    id: String,
    is_resolved: bool,
    resolved_by: Option<GqlActor>,
    is_outdated: bool,
    path: String,
    subject_type: Option<String>,
    line: Option<u32>,
    original_line: Option<u32>,
    start_line: Option<u32>,
    original_start_line: Option<u32>,
    diff_side: Option<String>,
    comments: Nodes<GqlComment>,
}

#[derive(Deserialize)]
struct Nodes<T> {
    nodes: Vec<T>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlComment {
    body: String,
    author: Option<GqlActor>,
    diff_hunk: String,
}

#[derive(Deserialize)]
struct GqlActor {
    login: String,
}

impl GqlThread {
    fn into_remote(self) -> RemoteThread {
        // `line` is gone once outdated; the original lines are where it was written.
        let (start, end) = match self.line {
            Some(line) => (self.start_line, Some(line)),
            None => (self.original_start_line, self.original_line),
        };
        let anchor = match (self.subject_type.as_deref(), end) {
            (Some("FILE"), _) | (_, None) => RemoteAnchor::File { path: self.path },
            (_, Some(end)) => {
                let side = if self.diff_side.as_deref() == Some("LEFT") { Side::Old } else { Side::New };
                RemoteAnchor::Lines { path: self.path, side, start: start.unwrap_or(end), end }
            }
        };
        let diff_hunk = self.comments.nodes.first().map(|c| c.diff_hunk.clone()).unwrap_or_default();
        let comments = self
            .comments
            .nodes
            .into_iter()
            // GitHub shows a deleted account as "ghost".
            .map(|c| RemoteComment { author: c.author.map_or_else(|| "ghost".to_owned(), |a| a.login), body: c.body })
            .collect();
        RemoteThread {
            id: self.id,
            anchor,
            outdated: self.is_outdated,
            // GitHub shows a deleted account as "ghost".
            resolved_by: self.is_resolved.then(|| self.resolved_by.map_or_else(|| "ghost".to_owned(), |a| a.login)),
            diff_hunk,
            comments,
        }
    }
}
