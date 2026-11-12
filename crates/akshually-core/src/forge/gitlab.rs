//! The GitLab adapter, for gitlab.com and self-hosted GitLab: REST v4 for merge requests, their
//! diffs and discussions.

use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use ureq::http::Response;
use ureq::{Body, RequestBuilder};
use url::Url;

use super::{
    CredentialSources, ForgeAdapter, ForgeError, ForgeKind, HostConfig, InboxItem, InboxRole, PullRequest,
    PullRequestRef, RemoteAnchor, RemoteComment, RemoteThread, Review, ReviewComment,
};
use super::{FileFetch, epoch_seconds, fetch_files, with_suggestion};
use crate::{Changeset, Hunk, Side, Verdict};

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
        cached.clone().ok_or_else(|| self.credentials.none_for(&self.host))
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

    /// Sends `request` with the token. Every token goes as a Bearer token: glab may hold an
    /// OAuth token, which needs one, and personal access tokens work either way.
    fn send<B>(
        &self,
        request: RequestBuilder<B>,
        send: impl FnOnce(RequestBuilder<B>) -> Result<Response<Body>, ureq::Error>,
    ) -> Result<Response<Body>, ForgeError> {
        let request = request.header("Authorization", format!("Bearer {}", self.token()?));
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

    fn get(&self, url: &Url) -> Result<Response<Body>, ForgeError> {
        self.send(self.agent.get(url.as_str()), RequestBuilder::call)
    }

    fn get_json<T: DeserializeOwned>(&self, url: &Url) -> Result<T, ForgeError> {
        self.get(url)?.body_mut().read_json().map_err(|e| self.unexpected(e))
    }

    /// `POST url` with the JSON `body`.
    fn post(&self, url: &Url, body: Value) -> Result<Response<Body>, ForgeError> {
        self.send(self.agent.post(url.as_str()), |r| r.send_json(body))
    }

    /// `PUT url` with the JSON `body`.
    fn put(&self, url: &Url, body: Value) -> Result<Response<Body>, ForgeError> {
        self.send(self.agent.put(url.as_str()), |r| r.send_json(body))
    }

    fn delete(&self, url: &Url) -> Result<Response<Body>, ForgeError> {
        self.send(self.agent.delete(url.as_str()), RequestBuilder::call)
    }

    /// GitLab's version, as `(major, minor)`.
    fn version(&self) -> Result<(u32, u32), ForgeError> {
        #[derive(Deserialize)]
        struct Answer {
            /// `19.2.0-ee`.
            version: String,
        }
        let answer: Answer = self.get_json(&self.endpoint(&["version"]))?;
        let mut numbers = answer.version.split(['.', '-']).map(str::parse::<u32>);
        match (numbers.next(), numbers.next()) {
            (Some(Ok(major)), Some(Ok(minor))) => Ok((major, minor)),
            _ => Err(self.unexpected(format!("a version of {:?}", answer.version))),
        }
    }

    /// The merge request's diff, file by file.
    fn diffs(&self, pr: &PullRequestRef) -> Result<Vec<Diff>, ForgeError> {
        self.pages(self.merge_request_url(pr, &["diffs"])).map_err(|e| self.not_found_as(e, pr))
    }

    /// Whether the human can approve `pr`, and whether they have.
    fn approvals(&self, pr: &PullRequestRef) -> Result<Approvals, ForgeError> {
        self.get_json(&self.merge_request_url(pr, &["approvals"])).map_err(|e| self.not_found_as(e, pr))
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
    /// The target branch's tip when GitLab made the diff.
    start_sha: String,
    head_sha: String,
}

/// One file of a merge request's diff, as `/diffs` lists it.
#[derive(Deserialize)]
struct Diff {
    old_path: String,
    new_path: String,
    new_file: bool,
    renamed_file: bool,
    deleted_file: bool,
    /// The file's unified diff, without the `---`/`+++` header; empty for a binary file, a
    /// pure rename, or a diff GitLab collapsed or found too large.
    #[serde(default)]
    diff: String,
}

/// The human's approval of a merge request.
#[derive(Deserialize)]
struct Approvals {
    /// GitLab's approval rules let them approve it now (false once they have).
    user_can_approve: bool,
    user_has_approved: bool,
}

/// GitLab 19.2 took `reviewer_state` in bulk publish: before, the API can't request changes.
const REQUEST_CHANGES_SINCE: (u32, u32) = (19, 2);

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
        let id = &pr.id;
        let listed = self.diffs(id)?;
        // Like the local Source, renames are a deletion plus an addition. GitLab takes line
        // comments only on the lines its diff shows, so each file keeps the diff's hunks (none
        // for a diff GitLab collapsed or found too large).
        let mut fetches = Vec::new();
        for diff in listed {
            let hunks = Some(Hunk::parse_patch(&diff.diff));
            let fetch = |path: &String, old: Option<&String>, new: Option<&String>| FileFetch {
                path: path.clone(),
                old: old.cloned(),
                new: new.cloned(),
                hunks: hunks.clone(),
            };
            let (old, new) = (&diff.old_path, &diff.new_path);
            if diff.new_file {
                fetches.push(fetch(new, None, Some(new)));
            } else if diff.deleted_file {
                fetches.push(fetch(old, Some(old), None));
            } else if diff.renamed_file {
                fetches.push(fetch(old, Some(old), None));
                fetches.push(fetch(new, None, Some(new)));
            } else {
                fetches.push(fetch(new, Some(old), Some(new)));
            }
        }
        fetches.sort_by(|a, b| a.path.cmp(&b.path));
        let files = fetch_files(fetches, |path, side| {
            self.file(id, path, if side == Side::Old { &pr.base_sha } else { &pr.head_sha })
        })?;
        Ok(Changeset { base: pr.base_sha.clone(), files })
    }

    fn threads(&self, pr: &PullRequest) -> Result<Vec<RemoteThread>, ForgeError> {
        let discussions: Vec<Discussion> =
            self.pages(self.merge_request_url(&pr.id, &["discussions"])).map_err(|e| self.not_found_as(e, &pr.id))?;
        Ok(discussions.into_iter().filter_map(|d| d.into_remote(&pr.head_sha)).collect())
    }

    /// Approving as GitLab's approval rules allow (approving again keeps an approval); requesting
    /// changes from GitLab 19.2, on someone else's merge request; feedback always.
    fn verdicts(&self, pr: &PullRequest) -> Result<Vec<Verdict>, ForgeError> {
        let version = self.version()?;
        let me: User = self.get_json(&self.endpoint(&["user"]))?;
        let approvals = self.approvals(&pr.id)?;
        let mut verdicts = Vec::new();
        if approvals.user_can_approve || approvals.user_has_approved {
            verdicts.push(Verdict::Approve);
        }
        if version >= REQUEST_CHANGES_SINCE && !me.username.eq_ignore_ascii_case(&pr.author) {
            verdicts.push(Verdict::RequestChanges);
        }
        verdicts.push(Verdict::Feedback);
        Ok(verdicts)
    }

    /// A draft note per new Thread, reply and the summary, then the Resolved changes and the
    /// approval, then bulk publish sends the draft notes with the reviewer state as one review.
    /// Everything that can be refused is checked before anything is sent: a push since `pr`
    /// was fetched, a Verdict or file comment this GitLab can't take, and draft notes the human
    /// left in the browser, which bulk publish would send too. A failure on the way deletes the
    /// draft notes and undoes the Resolved changes and the approval.
    fn publish(&self, pr: &PullRequest, review: &Review) -> Result<String, ForgeError> {
        let id = &pr.id;
        let mr: MergeRequest = self.get_json(&self.merge_request_url(id, &[])).map_err(|e| self.not_found_as(e, id))?;
        let refs = mr.diff_refs.ok_or_else(|| self.unexpected(format!("{id} has no diff yet; try again shortly")))?;
        if refs.head_sha != pr.head_sha {
            return Err(ForgeError::Moved(id.to_string()));
        }
        let version = self.version()?;
        if review.verdict == Verdict::RequestChanges && version < REQUEST_CHANGES_SINCE {
            return Err(self.unsupported("request changes before GitLab 19.2"));
        }
        let left: Vec<Value> = self.get_json(&self.merge_request_url(id, &["draft_notes"]))?;
        if !left.is_empty() {
            return Err(ForgeError::DraftsInBrowser { pr: id.to_string(), count: left.len() });
        }

        // The summary first, as a general comment, so the review reads in order.
        let mut notes = Vec::new();
        if !review.body.is_empty() {
            notes.push(json!({ "note": review.body }));
        }
        if !review.comments.is_empty() {
            let diffs = self.diffs(id)?;
            for comment in &review.comments {
                notes.push(self.new_thread(comment, &refs, &diffs, version)?);
            }
        }
        let replied = |thread: &String| review.replies.iter().any(|r| &r.thread == thread);
        for reply in &review.replies {
            let mut note = json!({ "note": reply.body, "in_reply_to_discussion_id": reply.thread });
            // The reply resolves its Thread as it's published.
            if review.resolve.contains(&reply.thread) {
                note["resolve_discussion"] = json!(true);
            }
            notes.push(note);
        }
        let resolutions = review.resolve.iter().filter(|t| !replied(t)).map(|t| (t.clone(), true));
        let resolutions = resolutions.chain(review.reopen.iter().map(|t| (t.clone(), false))).collect();
        // Approving again would be refused; the approval stands.
        let approve = review.verdict == Verdict::Approve && !self.approvals(id)?.user_has_approved;
        let reviewer_state = match review.verdict {
            Verdict::RequestChanges => Some("requested_changes"),
            Verdict::Feedback if version >= REQUEST_CHANGES_SINCE => Some("reviewed"),
            // Approving sets the reviewer state itself.
            _ => None,
        };
        let sending = Sending { notes, resolutions, approve_at: approve.then_some(refs.head_sha), reviewer_state };

        let mut done = Done::default();
        let sent = self.send_review(id, sending, &mut done);
        if sent.is_err() {
            self.undo(id, done);
        }
        sent.map(|()| mr.web_url)
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

/// GitLab took file comments (a `file` position) from 17.0.
const FILE_COMMENTS_SINCE: (u32, u32) = (17, 0);

/// What Publish sends, worked out before anything is.
struct Sending {
    /// The draft notes to create: the summary, new Threads, replies.
    notes: Vec<Value>,
    /// Threads to mark Resolved (`true`) or reopen, beyond those a reply resolves.
    resolutions: Vec<(String, bool)>,
    /// The head to approve at, when the Verdict approves and the human hasn't yet.
    approve_at: Option<String>,
    /// Bulk publish's `reviewer_state`, if any.
    reviewer_state: Option<&'static str>,
}

/// What Publish has sent so far, to undo after a failure.
#[derive(Default)]
struct Done {
    /// The draft notes created.
    notes: Vec<u64>,
    /// Each Thread whose Resolved state changed, and to what.
    resolved: Vec<(String, bool)>,
    approved: bool,
}

impl GitLab {
    /// The draft note opening a new Thread for `comment`, positioned on the merge request's
    /// version `refs` through its `diffs`.
    fn new_thread(
        &self,
        comment: &ReviewComment,
        refs: &DiffRefs,
        diffs: &[Diff],
        version: (u32, u32),
    ) -> Result<Value, ForgeError> {
        let (path, side) = match &comment.anchor {
            RemoteAnchor::Lines { path, side, .. } => (path, Some(*side)),
            RemoteAnchor::File { path } => (path, None),
        };
        let diff = diffs
            .iter()
            .find(|d| match side {
                Some(Side::Old) => d.old_path == *path && !d.new_file,
                Some(Side::New) => d.new_path == *path && !d.deleted_file,
                None => d.new_path == *path || d.old_path == *path,
            })
            .ok_or_else(|| self.unexpected(format!("{path} isn't in the merge request's diff")))?;
        let mut position = json!({
            "base_sha": refs.base_sha,
            "start_sha": refs.start_sha,
            "head_sha": refs.head_sha,
            "old_path": diff.old_path,
            "new_path": diff.new_path,
        });
        let mut note = comment.body.clone();
        match comment.anchor {
            RemoteAnchor::File { .. } => {
                if version < FILE_COMMENTS_SINCE {
                    return Err(self.unsupported("comment on a whole file before GitLab 17.0"));
                }
                position["position_type"] = json!("file");
            }
            RemoteAnchor::Lines { side, start, end, .. } => {
                position["position_type"] = json!("text");
                // The note sits on the last line; a range also names its first.
                let last = diff.line(side, end);
                if let Some(line) = last.old_line() {
                    position["old_line"] = json!(line);
                }
                if let Some(line) = last.new_line() {
                    position["new_line"] = json!(line);
                }
                if start < end {
                    let first = diff.line(side, start);
                    position["line_range"] = json!({ "start": diff.range_end(first), "end": diff.range_end(last) });
                }
                // GitLab's suggestion replaces the commented line and the given number above it.
                if let (Some(code), Side::New) = (&comment.suggestion, side) {
                    note = with_suggestion(&note, &format!("suggestion:-{}+0", end - start), code);
                }
            }
        }
        Ok(json!({ "note": note, "position": position }))
    }

    /// Creates the draft notes, makes the Resolved changes, approves, then bulk publishes the
    /// draft notes with the reviewer state. Records in `done` what it sent.
    fn send_review(&self, pr: &PullRequestRef, sending: Sending, done: &mut Done) -> Result<(), ForgeError> {
        #[derive(Deserialize)]
        struct Created {
            id: u64,
        }
        let draft_notes = self.merge_request_url(pr, &["draft_notes"]);
        for note in sending.notes {
            let created: Created =
                self.post(&draft_notes, note)?.body_mut().read_json().map_err(|e| self.unexpected(e))?;
            done.notes.push(created.id);
        }
        for (thread, resolved) in sending.resolutions {
            self.put(&self.merge_request_url(pr, &["discussions", &thread]), json!({ "resolved": resolved }))?;
            done.resolved.push((thread, resolved));
        }
        if let Some(head) = sending.approve_at {
            // At the head reviewed: GitLab refuses it if another push came meanwhile.
            self.post(&self.merge_request_url(pr, &["approve"]), json!({ "sha": head }))?;
            done.approved = true;
        }
        if !done.notes.is_empty() {
            let body = match sending.reviewer_state {
                Some(state) => json!({ "reviewer_state": state }),
                None => json!({}),
            };
            if let Err(error) = self.post(&self.merge_request_url(pr, &["draft_notes", "bulk_publish"]), body) {
                // Bulk publish may have gone through with its answer lost: undoing would
                // unapprove a review that went out, and a retry would post it twice. Publish
                // checked that no draft notes were left before; none now means they went out.
                // One GitLab can't say about is taken to have failed, as before asking.
                let published = error.may_have_gone_through()
                    && self
                        .get_json::<Vec<Value>>(&self.merge_request_url(pr, &["draft_notes"]))
                        .is_ok_and(|left| left.is_empty());
                if !published {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Best effort: the error that stopped Publish is the one to report.
    fn undo(&self, pr: &PullRequestRef, done: Done) {
        for note in done.notes {
            let _ = self.delete(&self.merge_request_url(pr, &["draft_notes", &note.to_string()]));
        }
        for (thread, resolved) in done.resolved {
            let _ = self.put(&self.merge_request_url(pr, &["discussions", &thread]), json!({ "resolved": !resolved }));
        }
        if done.approved {
            let _ = self.post(&self.merge_request_url(pr, &["unapprove"]), json!({}));
        }
    }

    fn unsupported(&self, what: &'static str) -> ForgeError {
        ForgeError::Unsupported { host: self.host.host.clone(), what }
    }
}

/// A line of a file's diff, located on both sides.
#[derive(Clone, Copy)]
struct DiffLine {
    /// `"new"` for an added line, `"old"` for a removed one, none for an unchanged one.
    kind: Option<&'static str>,
    /// The line's number on each side. An added line has none on the old side, and a removed
    /// one none on the new side: there, it's the number GitLab's line code counts.
    old: u32,
    new: u32,
}

impl DiffLine {
    fn old_line(&self) -> Option<u32> {
        (self.kind != Some("new")).then_some(self.old)
    }

    fn new_line(&self) -> Option<u32> {
        (self.kind != Some("old")).then_some(self.new)
    }
}

impl Diff {
    /// Line `line` on `side` of this file: in a hunk as the diff has it, else an unchanged line
    /// shifted by the lines added and removed before it.
    fn line(&self, side: Side, line: u32) -> DiffLine {
        let on = |old: u32, new: u32| match side {
            Side::Old => old,
            Side::New => new,
        };
        // The next line on each side, as GitLab counts them for line codes.
        let (mut old, mut new) = (0, 0);
        // How far the new side is ahead of the old past the hunks so far.
        let mut shift: i64 = 0;
        for text in self.diff.lines() {
            if let Some(hunk) = Hunk::parse_patch(text).first() {
                // A side without lines names the line before the hunk.
                let first = |start: u32, lines: u32| if lines == 0 { start + 1 } else { start };
                let (old_first, new_first) =
                    (first(hunk.old_start, hunk.old_lines), first(hunk.new_start, hunk.new_lines));
                if line < on(old_first, new_first) {
                    break;
                }
                (old, new) = (hunk.old_start, hunk.new_start);
                shift = i64::from(new_first + hunk.new_lines) - i64::from(old_first + hunk.old_lines);
                continue;
            }
            let at = on(old, new) == line;
            match text.as_bytes().first() {
                Some(b' ') if at => return DiffLine { kind: None, old, new },
                Some(b'-') if at && side == Side::Old => return DiffLine { kind: Some("old"), old, new },
                Some(b'+') if at && side == Side::New => return DiffLine { kind: Some("new"), old, new },
                Some(b' ') => (old, new) = (old + 1, new + 1),
                Some(b'-') => old += 1,
                Some(b'+') => new += 1,
                // `\ No newline at end of file`.
                _ => {}
            }
        }
        let other = |n: u32, by: i64| u32::try_from(i64::from(n) + by).unwrap_or(0);
        match side {
            Side::Old => DiffLine { kind: None, old: line, new: other(line, shift) },
            Side::New => DiffLine { kind: None, old: other(line, -shift), new: line },
        }
    }

    /// `line` as one end of a position's `line_range`.
    fn range_end(&self, line: DiffLine) -> Value {
        json!({
            "line_code": self.line_code(line),
            "type": line.kind,
            "old_line": line.old_line(),
            "new_line": line.new_line(),
        })
    }

    /// GitLab's name for a line of the diff: the SHA-1 of the file's path, then its old and new
    /// numbers.
    fn line_code(&self, line: DiffLine) -> String {
        let digest = Sha1::digest(self.new_path.as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        format!("{hex}_{}_{}", line.old, line.new)
    }
}
