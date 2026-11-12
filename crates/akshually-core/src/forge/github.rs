//! The GitHub adapter, for github.com and GitHub Enterprise Server: REST for the Pull Request
//! and its files, GraphQL for review Threads (REST lacks their resolved state).

use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use ureq::http::Response;
use ureq::{Body, RequestBuilder};
use url::Url;

use super::{
    CredentialSources, ForgeAdapter, ForgeError, ForgeKind, HostConfig, InboxItem, InboxRole, PullRequest,
    PullRequestRef, RemoteAnchor, RemoteComment, RemoteThread,
};
use super::{epoch_seconds, file_change};
use crate::{Changeset, Side};

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
