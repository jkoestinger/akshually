//! akshually's releases on GitHub: listing them, and downloading their assets.

use std::time::Duration;

use serde::Deserialize;

use super::{Asset, Release, UpdateError};

/// akshually's repository on the GitHub REST API, where its releases are listed.
pub const GITHUB_RELEASES_API: &str = "https://api.github.com/repos/jkoestinger/akshually";

/// How long one request about releases may take, download included: an asset is tens of
/// megabytes, which a slow connection takes minutes over.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The most read of a downloaded asset: far above any release's size, so a bad answer can't
/// fill the memory.
const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;

/// How many releases one lookup lists, newest first: GitHub's most per page, far more than will
/// ever be newer than an install.
const RELEASES_PER_PAGE: u32 = 100;

/// Requests to akshually's releases, unauthenticated: one lookup a day stays far within
/// GitHub's 60 requests an hour. Blocks on the network.
pub struct Releases {
    /// The repository's REST API base, without a trailing `/`.
    api: String,
    agent: ureq::Agent,
}

impl Releases {
    /// Releases of the repository whose REST API base is `api` (a trailing `/` is dropped).
    pub fn new(api: &str) -> Self {
        let agent = ureq::Agent::config_builder()
            // Read 4xx/5xx bodies for GitHub's error messages.
            .http_status_as_error(false)
            .timeout_global(Some(REQUEST_TIMEOUT))
            .user_agent(concat!("akshually/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Releases { api: api.trim_end_matches('/').to_owned(), agent }
    }

    /// akshually's own releases, or those at `AKSHUALLY_RELEASES_API` (for tests).
    pub fn from_env() -> Self {
        let api = std::env::var("AKSHUALLY_RELEASES_API").unwrap_or_else(|_| GITHUB_RELEASES_API.to_owned());
        Releases::new(&api)
    }

    /// The published releases, leaving out drafts and tags that aren't versions.
    pub fn list(&self) -> Result<Vec<Release>, UpdateError> {
        #[derive(Deserialize)]
        struct Listed {
            tag_name: String,
            #[serde(default)]
            draft: bool,
            #[serde(default)]
            assets: Vec<ListedAsset>,
        }
        #[derive(Deserialize)]
        struct ListedAsset {
            name: String,
            browser_download_url: String,
        }
        let url = format!("{}/releases", self.api);
        let request = self
            .agent
            .get(&url)
            .query("per_page", RELEASES_PER_PAGE.to_string())
            .header("Accept", "application/vnd.github+json");
        let body = self.read(request.call(), &url)?;
        let listed: Vec<Listed> = serde_json::from_slice(&body).map_err(|e| UpdateError::Unexpected(e.to_string()))?;
        let releases = listed.into_iter().filter(|r| !r.draft).filter_map(|r| {
            let assets = r.assets.into_iter().map(|a| Asset { name: a.name, url: a.browser_download_url }).collect();
            Some(Release { version: r.tag_name.parse().ok()?, tag: r.tag_name, assets })
        });
        Ok(releases.collect())
    }

    /// The contents of the asset at `url`, following GitHub's redirect to its storage.
    pub(super) fn download(&self, url: &str) -> Result<Vec<u8>, UpdateError> {
        self.read(self.agent.get(url).call(), url)
    }

    /// The body of a 2xx answer to the request for `url`; any other answer is an error.
    fn read(
        &self,
        response: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
        url: &str,
    ) -> Result<Vec<u8>, UpdateError> {
        let http = |e: ureq::Error| UpdateError::Http { url: url.to_owned(), message: e.to_string() };
        let mut response = response.map_err(http)?;
        let status = response.status().as_u16();
        let body = response.body_mut().with_config().limit(MAX_DOWNLOAD).read_to_vec().map_err(http)?;
        if !(200..300).contains(&status) {
            #[derive(Deserialize)]
            struct GitHubError {
                message: String,
            }
            let message = serde_json::from_slice::<GitHubError>(&body)
                .map_or_else(|_| String::from_utf8_lossy(&body).into_owned(), |e| e.message);
            return Err(UpdateError::Api { url: url.to_owned(), status, message });
        }
        Ok(body)
    }
}
