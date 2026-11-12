//! The GitHub adapter, for github.com and GitHub Enterprise Server: REST for the Pull Request
//! and its files, GraphQL for review Threads (REST lacks their resolved state).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use ureq::http::Response;
use ureq::{Body, RequestBuilder};
use url::Url;

use super::{
    CredentialSources, ForgeAdapter, ForgeError, ForgeKind, HostConfig, InboxItem, InboxRole, PullRequest,
    PullRequestRef, PullRequestUpdate, RemoteAnchor, RemoteComment, RemoteThread, Review,
};
use super::{FileFetch, epoch_seconds, fetch_files, with_suggestion};
use crate::{Changeset, Hunk, Side, Verdict};

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
    /// The last `update` answer per Pull Request, to poll again conditionally.
    polls: Mutex<HashMap<PullRequestRef, Polled>>,
}

/// What polling a Pull Request for pushes answered last.
struct Polled {
    /// The Pull Request's ETag then: GitHub answers 304 while it's unchanged, which doesn't
    /// count against the rate limit.
    etag: String,
    /// The head the poll compared against.
    since: String,
    update: Option<PullRequestUpdate>,
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
        GitHub { host, api, graphql, credentials, token: Mutex::new(None), agent, polls: Mutex::default() }
    }

    fn token(&self) -> Result<String, ForgeError> {
        let mut cached = self.token.lock().unwrap_or_else(|e| e.into_inner());
        if cached.is_none() {
            *cached = self.credentials.github_token(&self.host);
        }
        cached.clone().ok_or_else(|| self.credentials.none_for(&self.host))
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
        // 304 answers a conditional request: unchanged.
        if (200..300).contains(&status) || status == 304 {
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

    fn lock_polls(&self) -> std::sync::MutexGuard<'_, HashMap<PullRequestRef, Polled>> {
        self.polls.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pull_url(&self, pr: &PullRequestRef) -> Url {
        self.endpoint(&repo_segments(pr, &["pulls", &pr.number.to_string()]))
    }

    /// A 404 names the Pull Request.
    fn not_found_as(&self, error: ForgeError, pr: &PullRequestRef) -> ForgeError {
        match error {
            ForgeError::NotFound(_) => ForgeError::NotFound(pr.to_string()),
            e => e,
        }
    }

    /// How many commits `new` is ahead of `old`; `None` when GitHub can't say, as after a
    /// force-push whose old head is gone.
    fn commits_between(&self, pr: &PullRequestRef, old: &str, new: &str) -> Option<u32> {
        #[derive(Deserialize)]
        struct Compare {
            ahead_by: u32,
        }
        let range = format!("{old}...{new}");
        let url = self.endpoint(&repo_segments(pr, &["compare", &range]));
        self.get_json::<Compare>(url.as_str()).ok().map(|c| c.ahead_by)
    }

    fn http(&self, error: impl std::fmt::Display) -> ForgeError {
        ForgeError::Http { host: self.host.host.clone(), message: error.to_string() }
    }

    fn unexpected(&self, error: impl std::fmt::Display) -> ForgeError {
        ForgeError::Unexpected { host: self.host.host.clone(), message: error.to_string() }
    }
}

/// A Pull Request as GitHub's REST API answers it.
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

impl Pull {
    fn into_pull_request(self, pr: &PullRequestRef) -> PullRequest {
        PullRequest {
            id: pr.clone(),
            url: self.html_url,
            title: self.title,
            author: self.user.login,
            head_branch: self.head.name,
            head_sha: self.head.sha,
            base_sha: self.base.sha,
        }
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
        let pull: Pull = self.get_json(self.pull_url(pr).as_str()).map_err(|e| self.not_found_as(e, pr))?;
        Ok(pull.into_pull_request(pr))
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
            /// GitHub's diff of the file; none for a binary file or a huge diff.
            patch: Option<String>,
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

        // Like the local Source, renames are a deletion plus an addition. Line comments must
        // fit GitHub's own diff, so each file keeps its hunks.
        let mut fetches = Vec::new();
        for file in listed {
            let hunks = file.patch.as_deref().map(Hunk::parse_patch);
            let path = file.filename;
            let fetch = |old: Option<&String>, new: Option<&String>, hunks| FileFetch {
                path: new.or(old).cloned().unwrap_or_default(),
                old: old.cloned(),
                new: new.cloned(),
                hunks,
            };
            match file.status.as_str() {
                "added" | "copied" => fetches.push(fetch(None, Some(&path), hunks)),
                "removed" => fetches.push(fetch(Some(&path), None, hunks)),
                "renamed" => {
                    // GitHub shows the renamed file only: none of the old path's lines, and of
                    // the new path's only those its patch changes.
                    let previous = file.previous_filename.unwrap_or_else(|| path.clone());
                    fetches.push(fetch(Some(&previous), None, Some(Vec::new())));
                    fetches.push(fetch(None, Some(&path), Some(hunks.unwrap_or_default())));
                }
                "unchanged" => {}
                _ => fetches.push(fetch(Some(&path), Some(&path), hunks)),
            }
        }
        fetches.sort_by(|a, b| a.path.cmp(&b.path));
        let files = fetch_files(fetches, |path, side| {
            self.file(id, path, if side == Side::Old { &base } else { &pr.head_sha })
        })?;
        Ok(Changeset { base, files })
    }

    fn update(&self, pr: &PullRequest) -> Result<Option<PullRequestUpdate>, ForgeError> {
        let id = &pr.id;
        // Bound first, so the lock is released before the request.
        let etag = self.lock_polls().get(id).filter(|p| p.since == pr.head_sha).map(|p| p.etag.clone());
        let mut request = self.agent.get(self.pull_url(id).as_str());
        if let Some(etag) = &etag {
            request = request.header("If-None-Match", etag);
        }
        let mut response = self
            .send(request, "application/vnd.github+json", RequestBuilder::call)
            .map_err(|e| self.not_found_as(e, id))?;
        if response.status().as_u16() == 304 {
            let polled = self.lock_polls().get(id).filter(|p| p.since == pr.head_sha).map(|p| p.update.clone());
            if let Some(update) = polled {
                return Ok(update);
            }
            return Err(self.unexpected("304 Not Modified to a request that wasn't conditional"));
        }
        let etag = response.headers().get("etag").and_then(|v| v.to_str().ok()).map(str::to_owned);
        let pull: Pull = response.body_mut().read_json().map_err(|e| self.unexpected(e))?;
        let now = pull.into_pull_request(id);
        let update = (now.head_sha != pr.head_sha).then(|| PullRequestUpdate {
            commits: self.commits_between(id, &pr.head_sha, &now.head_sha),
            pull_request: now,
        });
        let mut polls = self.lock_polls();
        match etag {
            Some(etag) => {
                polls.insert(id.clone(), Polled { etag, since: pr.head_sha.clone(), update: update.clone() });
            }
            None => {
                polls.remove(id);
            }
        }
        Ok(update)
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

    fn inbox(&self) -> Result<Vec<InboxItem>, ForgeError> {
        // `review-requested` includes requests to the human's teams. A review removes the
        // request, so a reviewed Pull Request leaves the Inbox unless assigned.
        let mut items = self.search("review-requested:@me", InboxRole::Reviewer)?;
        for item in self.search("assignee:@me", InboxRole::Assignee)? {
            if !items.iter().any(|i| i.id == item.id) {
                items.push(item);
            }
        }
        Ok(items)
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

    /// A pending review with each new Thread and reply in it, then the Resolved changes, then
    /// the Verdict submits it. A failure on the way undoes the Resolved changes made and
    /// deletes the pending review, so nothing is half-published.
    fn publish(&self, pr: &PullRequest, review: &Review) -> Result<String, ForgeError> {
        let (owner, repo) = pr.id.repo.split_once('/').ok_or_else(|| self.unexpected("a repository without owner"))?;
        let variables = json!({ "owner": owner, "repo": repo, "number": pr.id.number });
        let data: Value = self.graphql(PULL_REQUEST_ID_QUERY, variables)?;
        let pr_id =
            data.pointer("/repository/pullRequest/id").ok_or_else(|| ForgeError::NotFound(pr.id.to_string()))?;
        let input = json!({ "pullRequestId": pr_id, "commitOID": pr.head_sha });
        let data: Value = self.graphql(START_REVIEW, json!({ "input": input }))?;
        let review_id = self.string_at(&data, "/addPullRequestReview/pullRequestReview/id")?;
        let mut changed = Vec::new();
        let submitted = self.fill(&review_id, review, &mut changed).and_then(|()| {
            self.submit(&review_id, review).or_else(|error| {
                // The submit may have gone through with its answer lost: undoing would reopen
                // Threads of a review that went out. Ask; only a review still pending failed,
                // and one GitHub can't say about is taken to have failed, as before asking.
                match error.may_have_gone_through().then(|| self.submitted_url(&review_id)) {
                    Some(Ok(Some(url))) => Ok(url),
                    _ => Err(error),
                }
            })
        });
        if submitted.is_err() {
            // Best effort: the error that stopped Publish is the one to report.
            for (thread, resolved) in changed {
                let _ = self.set_resolved(&thread, !resolved);
            }
            let input = json!({ "pullRequestReviewId": review_id });
            let _ = self.graphql::<Value>(DELETE_REVIEW, json!({ "input": input }));
        }
        submitted
    }
}

impl GitHub {
    /// The open Pull Requests matching `filter`, most recently updated first; the first 100,
    /// which is plenty for an Inbox.
    fn search(&self, filter: &str, role: InboxRole) -> Result<Vec<InboxItem>, ForgeError> {
        #[derive(Deserialize)]
        struct Found {
            items: Vec<Issue>,
        }
        #[derive(Deserialize)]
        struct Issue {
            repository_url: String,
            html_url: String,
            number: u64,
            title: String,
            user: Option<GqlActor>,
            created_at: String,
            updated_at: String,
        }
        let mut url = self.endpoint(&["search", "issues"]);
        url.query_pairs_mut()
            .append_pair("q", &format!("is:pr is:open archived:false {filter}"))
            .append_pair("sort", "updated")
            .append_pair("per_page", PAGE);
        let found: Found = self.get_json(url.as_str())?;
        found
            .items
            .into_iter()
            .map(|issue| {
                // `…/repos/OWNER/REPO`
                let mut path = issue.repository_url.rsplit('/');
                let (repo, owner) = (path.next().unwrap_or_default(), path.next().unwrap_or_default());
                let time = |t: &str| epoch_seconds(t).ok_or_else(|| self.unexpected(format!("a time of {t:?}")));
                Ok(InboxItem {
                    id: PullRequestRef {
                        host: self.host.host.clone(),
                        repo: format!("{owner}/{repo}"),
                        number: issue.number,
                    },
                    kind: ForgeKind::GitHub,
                    url: issue.html_url,
                    title: issue.title,
                    // GitHub shows a deleted account as "ghost".
                    author: issue.user.map_or_else(|| "ghost".to_owned(), |u| u.login),
                    created_at: time(&issue.created_at)?,
                    updated_at: time(&issue.updated_at)?,
                    role,
                })
            })
            .collect()
    }
}

impl GitHub {
    /// Adds `review`'s Threads and replies to the pending review `review_id`, and makes its
    /// Resolved changes. Records in `changed` each Thread whose Resolved state it changed, and
    /// to what.
    fn fill(&self, review_id: &str, review: &Review, changed: &mut Vec<(String, bool)>) -> Result<(), ForgeError> {
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
                Some(code) => with_suggestion(&comment.body, "suggestion", code),
                None => comment.body.clone(),
            });
            let data: Value = self.graphql(ADD_THREAD, json!({ "input": input }))?;
            self.string_at(&data, "/addPullRequestReviewThread/thread/id")?;
        }
        for reply in &review.replies {
            let input = json!({
                "pullRequestReviewId": review_id,
                "pullRequestReviewThreadId": reply.thread,
                "body": reply.body,
            });
            let data: Value = self.graphql(ADD_REPLY, json!({ "input": input }))?;
            self.string_at(&data, "/addPullRequestReviewThreadReply/comment/id")?;
        }
        // Before the submit, so a refusal still leaves nothing published.
        let resolutions = review.resolve.iter().map(|t| (t, true)).chain(review.reopen.iter().map(|t| (t, false)));
        for (thread, resolved) in resolutions {
            self.set_resolved(thread, resolved)?;
            changed.push((thread.clone(), resolved));
        }
        Ok(())
    }

    /// Submits the pending review `review_id` with the Verdict and body, returning the review's
    /// web page.
    fn submit(&self, review_id: &str, review: &Review) -> Result<String, ForgeError> {
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

    /// The web page of the review `review_id` if it was submitted, `None` while it's pending.
    fn submitted_url(&self, review_id: &str) -> Result<Option<String>, ForgeError> {
        let data: Value = self.graphql(REVIEW_STATE_QUERY, json!({ "id": review_id }))?;
        let state = self.string_at(&data, "/node/state")?;
        if state == "PENDING" {
            return Ok(None);
        }
        self.string_at(&data, "/node/url").map(Some)
    }

    /// Marks the Thread `thread` Resolved, or reopens it.
    fn set_resolved(&self, thread: &str, resolved: bool) -> Result<(), ForgeError> {
        let (mutation, field) = if resolved {
            (RESOLVE_THREAD, "resolveReviewThread")
        } else {
            (UNRESOLVE_THREAD, "unresolveReviewThread")
        };
        let data: Value = self.graphql(mutation, json!({ "input": { "threadId": thread } }))?;
        self.string_at(&data, &format!("/{field}/thread/id")).map(drop)
    }

    /// The string at `pointer` in a GraphQL answer.
    fn string_at(&self, data: &Value, pointer: &str) -> Result<String, ForgeError> {
        data.pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| self.unexpected(format!("GraphQL answered no {pointer}")))
    }
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

const ADD_REPLY: &str = "mutation($input: AddPullRequestReviewThreadReplyInput!) {
  addPullRequestReviewThreadReply(input: $input) { comment { id } }
}";

const RESOLVE_THREAD: &str = "mutation($input: ResolveReviewThreadInput!) {
  resolveReviewThread(input: $input) { thread { id } }
}";

const UNRESOLVE_THREAD: &str = "mutation($input: UnresolveReviewThreadInput!) {
  unresolveReviewThread(input: $input) { thread { id } }
}";

const SUBMIT_REVIEW: &str = "mutation($input: SubmitPullRequestReviewInput!) {
  submitPullRequestReview(input: $input) { pullRequestReview { url } }
}";

const REVIEW_STATE_QUERY: &str = "query($id: ID!) {
  node(id: $id) { ... on PullRequestReview { state url } }
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
