use crate::{
    UpdateError, UpdateErrorCode as Code,
    archive::check_disk_space,
    config::{AutoCheckPolicy, RepoSlug},
};
use reqwest::{
    Client, Response, StatusCode, Url,
    header::{self, HeaderMap, HeaderValue},
};
use sha2::{Digest, Sha256};
use std::{future::Future, path::Path, time::Duration};
use tokio::io::{AsyncWrite, AsyncWriteExt};

pub const MAX_JSON_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;
const API_HOST: &str = "api.github.com";
const FETCH_HOSTS: &[&str] = &[
    API_HOST,
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "github-releases.githubusercontent.com",
];

fn disallowed() -> UpdateError {
    UpdateError::new(
        Code::DisallowedUrlOrHost,
        "URL is not an allowed GitHub HTTPS destination",
    )
}

fn validate_url_text(raw: &str) -> Result<(), UpdateError> {
    // URL parsing can normalize backslashes and remove empty userinfo.
    if raw.contains('\\') || raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(disallowed());
    }
    Ok(())
}

pub fn validate_fetch_url(raw: &str) -> Result<Url, UpdateError> {
    validate_url_text(raw)?;
    let (scheme, rest) = raw.split_once("://").ok_or_else(disallowed)?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if !scheme.eq_ignore_ascii_case("https") || authority.is_empty() || authority.contains('@') {
        return Err(disallowed());
    }
    let url = Url::parse(raw).map_err(|_| disallowed())?;
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || !url
            .host_str()
            .is_some_and(|host| FETCH_HOSTS.contains(&host))
    {
        return Err(disallowed());
    }
    Ok(url)
}

/// hops_taken counts completed redirects, not the initial request.
pub fn next_redirect_headers(
    from: &Url,
    to: &Url,
    headers: &HeaderMap,
    hops_taken: usize,
) -> Result<HeaderMap, UpdateError> {
    if hops_taken >= MAX_REDIRECTS {
        return Err(UpdateError::new(
            Code::RedirectLimitExceeded,
            "GitHub redirect limit exceeded",
        ));
    }
    validate_fetch_url(to.as_str())?;
    let mut next = headers.clone();
    if from.host_str() != to.host_str() {
        next.remove(header::AUTHORIZATION);
    }
    Ok(next)
}

pub fn token_for_host<'a>(host: &str, initial: bool, token: Option<&'a str>) -> Option<&'a str> {
    token.filter(|token| initial && host == API_HOST && !token.is_empty())
}

#[derive(Debug, Eq, PartialEq)]
pub enum FetchResult<T> {
    Disabled,
    NotModified { etag: Option<String> },
    Modified { etag: Option<String>, body: T },
}

async fn automatic<T, F: Future<Output = Result<FetchResult<T>, UpdateError>>>(
    policy: AutoCheckPolicy,
    check: impl FnOnce() -> F,
) -> Result<FetchResult<T>, UpdateError> {
    match policy {
        AutoCheckPolicy::Off => Ok(FetchResult::Disabled),
        AutoCheckPolicy::On => check().await,
    }
}

pub async fn auto_check(
    policy: AutoCheckPolicy,
    repo: &RepoSlug,
    etag: Option<&str>,
) -> Result<FetchResult<Vec<u8>>, UpdateError> {
    automatic(policy, || async {
        fetch_json(
            &format!("https://{API_HOST}/repos/{}/releases", repo.as_str()),
            etag,
        )
        .await
    })
    .await
}

fn network(message: &str) -> UpdateError {
    UpdateError::new(Code::NetworkError, message)
}

fn header_text(headers: &HeaderMap, name: impl header::AsHeaderName) -> Option<&str> {
    headers.get(name)?.to_str().ok()
}

async fn fetch_response(
    url: &str,
    etag: Option<&str>,
    accept: &'static str,
) -> Result<Response, UpdateError> {
    let mut url = validate_fetch_url(url)?;
    let client = Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .user_agent(concat!("ddg-agents/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|_| network("Could not initialize the GitHub HTTPS client"))?;

    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT, HeaderValue::from_static(accept));
    if let Some(etag) = etag {
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_str(etag).map_err(|_| network("Invalid ETag"))?,
        );
    }
    if url.host_str() == Some(API_HOST)
        && let Ok(token) = std::env::var("GITHUB_TOKEN")
        && let Some(token) = token_for_host(API_HOST, true, Some(&token))
    {
        let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| network("Invalid GitHub authentication header"))?;
        value.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, value);
    }

    let mut hops = 0;
    loop {
        let response = client
            .get(url.clone())
            .headers(headers.clone())
            .send()
            .await
            .map_err(|_| network("GitHub HTTPS request failed"))?;
        if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            return Ok(response);
        }
        if hops >= MAX_REDIRECTS {
            return Err(UpdateError::new(
                Code::RedirectLimitExceeded,
                "GitHub redirect limit exceeded",
            ));
        }
        let location = header_text(response.headers(), header::LOCATION).ok_or_else(disallowed)?;
        validate_url_text(location)?;
        let next = if location.starts_with("//") {
            validate_fetch_url(&format!("https:{location}"))?
        } else if location
            .split(['/', '?', '#'])
            .next()
            .is_some_and(|s| s.contains(':'))
        {
            validate_fetch_url(location)?
        } else {
            url.join(location).map_err(|_| disallowed())?
        };
        headers = next_redirect_headers(&url, &next, &headers, hops)?;
        // The updater's environment token is initial-request only, even on same-host hops.
        headers.remove(header::AUTHORIZATION);
        url = next;
        hops += 1;
    }
}

fn response_not_modified(status: StatusCode, headers: &HeaderMap) -> Result<bool, UpdateError> {
    match status {
        StatusCode::NOT_MODIFIED => Ok(true),
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => {
            let reset =
                header_text(headers, "x-ratelimit-reset").and_then(|s| s.parse::<u64>().ok());
            let message = match reset {
                Some(time) => format!("GitHub rate limited requests; resets at Unix time {time}"),
                None => "GitHub rate limited requests".to_owned(),
            };
            let mut error = UpdateError::new(Code::RateLimited, message);
            error.rate_limit_reset_at = reset;
            Err(error)
        }
        StatusCode::NOT_FOUND => Err(UpdateError::new(
            Code::AssetNotFound,
            "GitHub resource not found",
        )),
        StatusCode::OK => Ok(false),
        _ => Err(network("Unexpected GitHub response status")),
    }
}

fn check_length(len: u64, limit: u64) -> Result<(), UpdateError> {
    if len > limit {
        return Err(UpdateError::new(
            Code::ResponseTooLarge,
            "GitHub response exceeds the size limit",
        ));
    }
    Ok(())
}

async fn read_bounded<B: AsRef<[u8]>>(
    output: &mut (impl AsyncWrite + Unpin),
    limit: u64,
    mut next: impl AsyncFnMut() -> Result<Option<B>, UpdateError>,
) -> Result<(), UpdateError> {
    let mut size = 0;
    while let Some(chunk) = next().await? {
        let bytes = chunk.as_ref();
        size += bytes.len() as u64;
        check_length(size, limit)?;
        output
            .write_all(bytes)
            .await
            .map_err(|e| UpdateError::new(Code::InstallDirNotWritable, e.to_string()))?;
    }
    output
        .flush()
        .await
        .map_err(|e| UpdateError::new(Code::InstallDirNotWritable, e.to_string()))
}

async fn read_body(
    response: &mut Response,
    output: &mut (impl AsyncWrite + Unpin),
    limit: u64,
) -> Result<(), UpdateError> {
    check_length(response.content_length().unwrap_or(0), limit)?;
    read_bounded(output, limit, async || {
        response
            .chunk()
            .await
            .map_err(|_| network("Could not read the GitHub response"))
    })
    .await
}

pub async fn fetch_json(
    url: &str,
    etag: Option<&str>,
) -> Result<FetchResult<Vec<u8>>, UpdateError> {
    let mut response = fetch_response(url, etag, "application/vnd.github+json").await?;
    let updated_etag = header_text(response.headers(), header::ETAG);
    if response_not_modified(response.status(), response.headers())? {
        return Ok(FetchResult::NotModified {
            etag: updated_etag.or(etag).map(str::to_owned),
        });
    }
    let etag = updated_etag.map(str::to_owned);
    let mut body = Vec::new();
    read_body(&mut response, &mut body, MAX_JSON_BYTES).await?;
    Ok(FetchResult::Modified { etag, body })
}

/// Download and verify a new file in a caller-owned private staging directory.
pub async fn download_archive(
    url: &str,
    path: &Path,
    expected_sha256: &str,
) -> Result<(), UpdateError> {
    let expected = crate::integrity::decode_sha256(expected_sha256)
        .ok_or_else(|| UpdateError::new(Code::ManifestInvalid, "Invalid SHA-256 digest"))?;
    let mut response = fetch_response(url, None, "application/octet-stream").await?;
    if response_not_modified(response.status(), response.headers())? {
        return Err(network("Unexpected 304 for archive download"));
    }
    let size = response.content_length().unwrap_or(MAX_ARCHIVE_BYTES);
    check_length(size, MAX_ARCHIVE_BYTES)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            UpdateError::new(
                Code::InstallDirNotWritable,
                "Archive requires a staging directory",
            )
        })?;
    check_disk_space(parent, size)?;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .map_err(|e| UpdateError::new(Code::InstallDirNotWritable, e.to_string()))?;
    struct Pending<'a> {
        file: Option<tokio::fs::File>,
        path: &'a Path,
    }
    impl Drop for Pending<'_> {
        fn drop(&mut self) {
            if self.file.take().is_some() {
                let _ = std::fs::remove_file(self.path);
            }
        }
    }
    let mut pending = Pending {
        file: Some(tokio::fs::File::from_std(file)),
        path,
    };
    let mut hasher = Sha256::new();
    read_bounded(
        pending.file.as_mut().unwrap(),
        MAX_ARCHIVE_BYTES,
        async || {
            let chunk = response
                .chunk()
                .await
                .map_err(|_| network("Could not read the GitHub response"))?;
            if let Some(bytes) = &chunk {
                hasher.update(bytes);
            }
            Ok(chunk)
        },
    )
    .await?;
    crate::integrity::verify_digest(hasher.finalize().as_ref(), expected)?;
    pending.file.take();
    Ok(())
}

#[cfg(test)]
#[path = "tests_http.rs"]
mod tests;
