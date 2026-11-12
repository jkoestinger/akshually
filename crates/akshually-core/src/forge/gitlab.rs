//! The GitLab adapter, for gitlab.com and self-hosted GitLab: REST v4 for merge requests, their
//! diffs and discussions.

use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use ureq::Body;
use ureq::http::Response;
use url::Url;

use super::{
    CredentialSources, ForgeAdapter, ForgeError, ForgeKind, HostConfig, InboxItem, InboxRole, PullRequest,
    PullRequestRef, RemoteAnchor, RemoteComment, RemoteThread, Review,
};
use super::{epoch_seconds, file_change};
use crate::{Changeset, Side, Verdict};

/// Pages of up to this many items, GitLab's maximum.
const PAGE: &str = "100";
/// The most of a file read for its contents.
const MAX_FILE: u64 = 100 * 1024 * 1024;

pub struct GitLab {
    host: HostConfig,
    /// The REST API's base URL, without a trailing `/`.
    api: String,
    /// The path GitLab is installed under (`/gitlab`), as segments; empty at the root.
    root: Vec<String>,
    credentials: CredentialSources,
    /// Resolved on first use; forgotten when the host refuses it, so a fresh login is picked up.
    token: Mutex<Option<String>>,
    agent: ureq::Agent,
}

impl GitLab {
    /// The adapter for `host`, whose API is at `host.api_base` or else `https://HOST/api/v4`. An
    /// `api_base` ending in `/api/v4` also gives the relative root GitLab's web pages are under.
    pub fn new(host: HostConfig, credentials: CredentialSources) -> Self {
        let api = match &host.api_base {
            Some(base) => base.trim_end_matches('/').to_owned(),
            None => format!("https://{}/api/v4", host.host),
        };
        let root = Url::parse(&api)
            .ok()
            .and_then(|url| {
                let path = url.path().trim_end_matches('/').strip_suffix("/api/v4")?.to_owned();
                Some(path.split('/').filter(|s| !s.is_empty()).map(str::to_owned).collect())
            })
            .unwrap_or_default();
        let agent = ureq::Agent::config_builder()
            // Read 4xx/5xx bodies for GitLab's error messages.
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(120)))
            .user_agent(concat!("akshually/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        GitLab { host, api, root, credentials, token: Mutex::new(None), agent }
    }

    fn token(&self) -> Result<String, ForgeError> {
        let mut cached = self.token.lock().unwrap_or_else(|e| e.into_inner());
        if cached.is_none() {
            *cached = self.credentials.gitlab_token(&self.host);
        }
        cached.clone().ok_or_else(|| ForgeError::NoCredentials { host: self.host.host.clone() })
    }

    /// `segments` appended to the REST base, each percent-encoded (so a project path's `/`
    /// becomes `%2F`, as GitLab wants).
    fn endpoint(&self, segments: &[&str]) -> Url {
        let mut url = Url::parse(&self.api).expect("the API base is a URL");
        url.path_segments_mut().expect("the API base is a URL with a path").pop_if_empty().extend(segments);
        url
    }

    /// `projects/:id/merge_requests/:iid` followed by `rest`.
    fn merge_request_url(&self, pr: &PullRequestRef, rest: &[&str]) -> Url {
        let iid = pr.number.to_string();
        let mut segments = vec!["projects", pr.repo.as_str(), "merge_requests", iid.as_str()];
        segments.extend(rest);
        self.endpoint(&segments)
    }

    /// `GET url`. Every token goes as a Bearer token: glab may hold an OAuth token, which
    /// needs one, and personal access tokens work either way.
    fn get(&self, url: &Url) -> Result<Response<Body>, ForgeError> {
        let request = self.agent.get(url.as_str()).header("Authorization", format!("Bearer {}", self.token()?));
        let mut response = request.call().map_err(|e| self.http(e))?;
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
                    #[serde(alias = "error")]
                    message: serde_json::Value,
                }
                let message = match serde_json::from_str::<Message>(&body) {
                    Ok(Message { message: serde_json::Value::String(m) }) => m,
                    Ok(Message { message }) => message.to_string(),
                    Err(_) => body,
                };
                ForgeError::Api { host, status, message }
            }
        })
    }

    fn get_json<T: DeserializeOwned>(&self, url: &Url) -> Result<T, ForgeError> {
        self.get(url)?.body_mut().read_json().map_err(|e| self.unexpected(e))
    }

    /// Every item of a paginated list at `url`, following GitLab's `x-next-page` header (on
    /// this API, so the token stays on this host).
    fn pages<T: DeserializeOwned>(&self, url: Url) -> Result<Vec<T>, ForgeError> {
        let mut items = Vec::new();
        let mut page = "1".to_owned();
        loop {
            let mut next = url.clone();
            next.query_pairs_mut().append_pair("per_page", PAGE).append_pair("page", &page);
            let mut response = self.get(&next)?;
            let following = response.headers().get("x-next-page").and_then(|v| v.to_str().ok()).map(str::trim);
            let following = following.filter(|p| !p.is_empty()).map(str::to_owned);
            items.extend(response.body_mut().read_json::<Vec<T>>().map_err(|e| self.unexpected(e))?);
            match following {
                Some(p) if p != page => page = p,
                _ => return Ok(items),
            }
        }
    }

    /// A file's contents at `sha`.
    fn file(&self, pr: &PullRequestRef, path: &str, sha: &str) -> Result<Vec<u8>, ForgeError> {
        let mut url = self.endpoint(&["projects", &pr.repo, "repository", "files", path, "raw"]);
        url.query_pairs_mut().append_pair("ref", sha);
        self.get(&url)?.body_mut().with_config().limit(MAX_FILE).read_to_vec().map_err(|e| self.http(e))
    }

    /// A 404 names the Pull Request.
    fn not_found_as(&self, error: ForgeError, pr: &PullRequestRef) -> ForgeError {
        match error {
            ForgeError::NotFound(_) => ForgeError::NotFound(pr.to_string()),
            e => e,
        }
    }

    fn http(&self, error: impl std::fmt::Display) -> ForgeError {
        ForgeError::Http { host: self.host.host.clone(), message: error.to_string() }
    }

    fn unexpected(&self, error: impl std::fmt::Display) -> ForgeError {
        ForgeError::Unexpected { host: self.host.host.clone(), message: error.to_string() }
    }
}

/// A merge request as GitLab's REST API answers it.
#[derive(Deserialize)]
struct MergeRequest {
    web_url: String,
    title: String,
    author: User,
    source_branch: String,
    /// Null while GitLab is still preparing the diff.
    diff_refs: Option<DiffRefs>,
}

#[derive(Deserialize)]
struct User {
    username: String,
}

#[derive(Deserialize)]
struct DiffRefs {
    /// The merge-base.
    base_sha: String,
    head_sha: String,
}

/// A discussion on a merge request: a Thread when it's on the diff.
#[derive(Deserialize)]
struct Discussion {
    id: String,
    notes: Vec<Note>,
}

#[derive(Deserialize)]
struct Note {
    body: String,
    /// Null for a deleted account.
    author: Option<User>,
    #[serde(default)]
    system: bool,
    #[serde(default)]
    resolved: bool,
    resolved_by: Option<User>,
    position: Option<Position>,
}

#[derive(Deserialize)]
struct Position {
    /// The merge request version the position is on.
    head_sha: String,
    old_path: String,
    new_path: String,
    position_type: String,
    old_line: Option<u32>,
    new_line: Option<u32>,
    line_range: Option<LineRange>,
}

#[derive(Deserialize)]
struct LineRange {
    start: RangeEnd,
    end: RangeEnd,
}

#[derive(Deserialize)]
struct RangeEnd {
    old_line: Option<u32>,
    new_line: Option<u32>,
}

/// GitLab shows a deleted account as "ghost".
fn login(user: Option<User>) -> String {
    user.map_or_else(|| "ghost".to_owned(), |u| u.username)
}

impl Discussion {
    /// The Thread, if the discussion is on the diff (general comments on the merge request
    /// aren't). `head_sha` is the merge request's current head: a position on another version
    /// is outdated.
    fn into_remote(self, head_sha: &str) -> Option<RemoteThread> {
        let mut notes = self.notes.into_iter().filter(|n| !n.system).peekable();
        let first = notes.peek()?;
        let position = first.position.as_ref()?;
        let anchor = position.anchor()?;
        let outdated = position.head_sha != head_sha;
        let resolved = first.resolved;
        let mut resolved_by = None;
        let mut comments = Vec::new();
        for note in notes {
            if resolved && resolved_by.is_none() && note.resolved {
                resolved_by = Some(login(note.resolved_by));
            }
            comments.push(RemoteComment { author: login(note.author), body: note.body });
        }
        Some(RemoteThread { id: self.id, anchor, outdated, resolved_by, diff_hunk: String::new(), comments })
    }
}

impl Position {
    /// Added lines have only a new line, removed ones only an old line, unchanged ones both;
    /// a range ends on its `end`.
    fn anchor(&self) -> Option<RemoteAnchor> {
        match self.position_type.as_str() {
            "text" => {}
            // An image's position is a point on it, the file as far as a Thread goes.
            _ => return Some(RemoteAnchor::File { path: self.new_path.clone() }),
        }
        let (start, end) = match &self.line_range {
            Some(range) => ((range.start.old_line, range.start.new_line), (range.end.old_line, range.end.new_line)),
            None => ((self.old_line, self.new_line), (self.old_line, self.new_line)),
        };
        Some(match end {
            (_, Some(end)) => RemoteAnchor::Lines {
                path: self.new_path.clone(),
                side: Side::New,
                start: start.1.filter(|s| *s <= end).unwrap_or(end),
                end,
            },
            (Some(end), None) => RemoteAnchor::Lines {
                path: self.old_path.clone(),
                side: Side::Old,
                start: start.0.filter(|s| *s <= end).unwrap_or(end),
                end,
            },
            (None, None) => return None,
        })
    }
}

impl ForgeAdapter for GitLab {
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
        let segments = segments.strip_prefix(self.root.iter().map(String::as_str).collect::<Vec<_>>().as_slice())?;
        // Groups nest, so the project path is everything before `/-/merge_requests/IID`.
        let at = segments.windows(2).position(|w| w == ["-", "merge_requests"])?;
        let project = &segments[..at];
        if project.len() < 2 || project.iter().any(|s| s.is_empty()) {
            return None;
        }
        Some(PullRequestRef {
            host: self.host.host.clone(),
            repo: project.join("/"),
            number: segments.get(at + 2)?.parse().ok()?,
        })
    }

    fn pull_request(&self, pr: &PullRequestRef) -> Result<PullRequest, ForgeError> {
        let mr: MergeRequest = self.get_json(&self.merge_request_url(pr, &[])).map_err(|e| self.not_found_as(e, pr))?;
        let refs = mr.diff_refs.ok_or_else(|| self.unexpected(format!("{pr} has no diff yet; try again shortly")))?;
        Ok(PullRequest {
            id: pr.clone(),
            url: mr.web_url,
            title: mr.title,
            author: mr.author.username,
            head_branch: mr.source_branch,
            head_sha: refs.head_sha,
            // GitLab's diff is against the merge-base, which it names.
            base_sha: refs.base_sha,
        })
    }

    fn changeset(&self, pr: &PullRequest) -> Result<Changeset, ForgeError> {
        #[derive(Deserialize)]
        struct Diff {
            old_path: String,
            new_path: String,
            new_file: bool,
            renamed_file: bool,
            deleted_file: bool,
        }
        let id = &pr.id;
        let listed: Vec<Diff> = self.pages(self.merge_request_url(id, &["diffs"]))?;
        let old = |path: &str| self.file(id, path, &pr.base_sha).map(Some);
        let new = |path: &str| self.file(id, path, &pr.head_sha).map(Some);
        // Like the local Source, renames are a deletion plus an addition.
        let mut files = Vec::new();
        for diff in listed {
            if diff.new_file {
                files.push(file_change(diff.new_path.clone(), None, new(&diff.new_path)?));
            } else if diff.deleted_file {
                files.push(file_change(diff.old_path.clone(), old(&diff.old_path)?, None));
            } else if diff.renamed_file {
                files.push(file_change(diff.old_path.clone(), old(&diff.old_path)?, None));
                files.push(file_change(diff.new_path.clone(), None, new(&diff.new_path)?));
            } else {
                files.push(file_change(diff.new_path.clone(), old(&diff.old_path)?, new(&diff.new_path)?));
            }
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Changeset { base: pr.base_sha.clone(), files })
    }

    fn threads(&self, pr: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
        let discussions: Vec<Discussion> =
            self.pages(self.merge_request_url(&pr.id, &["discussions"])).map_err(|e| self.not_found_as(e, &pr.id))?;
        Ok(discussions.into_iter().filter_map(|d| d.into_remote(&pr.head_sha)).collect())
    }

    // Publishing to GitLab (draft notes, then bulk publish and approve) is ticket #28.
    fn verdicts(&self, _pr: &PullRequest) -> Result<Vec<Verdict>, ForgeError> {
        Err(ForgeError::Unsupported { host: self.host.host.clone(), what: "publish a review" })
    }

    fn publish(&self, _pr: &PullRequest, _review: &Review) -> Result<String, ForgeError> {
        Err(ForgeError::Unsupported { host: self.host.host.clone(), what: "publish a review" })
    }

    fn inbox(&self) -> Result<Vec<InboxItem>, ForgeError> {
        let me: User = self.get_json(&self.endpoint(&["user"]))?;
        let mut items = self.merge_requests("reviewer_username", &me.username, InboxRole::Reviewer)?;
        for item in self.merge_requests("assignee_username", &me.username, InboxRole::Assignee)? {
            if !items.iter().any(|i| i.id == item.id) {
                items.push(item);
            }
        }
        Ok(items)
    }
}

impl GitLab {
    /// The open merge requests, in any project, whose `filter` is `username`, most recently
    /// updated first; the first 100, which is plenty for an Inbox.
    fn merge_requests(&self, filter: &str, username: &str, role: InboxRole) -> Result<Vec<InboxItem>, ForgeError> {
        #[derive(Deserialize)]
        struct Listed {
            iid: u64,
            title: String,
            author: Option<User>,
            web_url: String,
            references: References,
            created_at: String,
            updated_at: String,
        }
        #[derive(Deserialize)]
        struct References {
            /// `group/project!iid`.
            full: String,
        }
        let mut url = self.endpoint(&["merge_requests"]);
        url.query_pairs_mut()
            .append_pair("state", "opened")
            // The default scope is the human's own merge requests.
            .append_pair("scope", "all")
            .append_pair(filter, username)
            .append_pair("order_by", "updated_at")
            .append_pair("per_page", PAGE);
        let listed: Vec<Listed> = self.get_json(&url)?;
        listed
            .into_iter()
            .map(|mr| {
                let repo = mr.references.full.rsplit_once('!').map_or(mr.references.full.as_str(), |(p, _)| p);
                let time = |t: &str| epoch_seconds(t).ok_or_else(|| self.unexpected(format!("a time of {t:?}")));
                Ok(InboxItem {
                    id: PullRequestRef { host: self.host.host.clone(), repo: repo.to_owned(), number: mr.iid },
                    kind: ForgeKind::GitLab,
                    url: mr.web_url,
                    title: mr.title,
                    author: login(mr.author),
                    created_at: time(&mr.created_at)?,
                    updated_at: time(&mr.updated_at)?,
                    role,
                })
            })
            .collect()
    }
}
