use super::*;

#[tokio::test]
async fn http_guard_urls_redirects_and_opt_out() {
    for host in [
        "api.github.com",
        "github.com",
        "objects.githubusercontent.com",
        "release-assets.githubusercontent.com",
        "github-releases.githubusercontent.com",
    ] {
        assert!(validate_fetch_url(&format!("https://{host}:443/release")).is_ok());
        assert_eq!(
            token_for_host(host, true, Some("token")),
            (host == API_HOST).then_some("token")
        );
        assert_eq!(token_for_host(host, false, Some("token")), None);
    }
    for invalid in [
        "http://api.github.com",
        "https://github.com:444",
        "https://github.com.evil.example",
        "https://evil.github.com",
        "https://user:pass@github.com",
        "https://@github.com",
        "https://127.0.0.1",
        "https://[::1]",
        "https://github.com./",
        "https://api.github.com\\x",
        "https://api.github.com/\n",
        "file:///manifest.json",
        "https:////@api.github.com",
    ] {
        assert!(validate_fetch_url(invalid).is_err(), "{invalid}");
    }
    let from = validate_fetch_url("https://api.github.com/a").unwrap();
    let same = validate_fetch_url("https://api.github.com/b").unwrap();
    let other = validate_fetch_url("https://release-assets.githubusercontent.com/b").unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer token"),
    );
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static("application/octet-stream"),
    );
    for hops in 0..5 {
        assert!(
            next_redirect_headers(&from, &same, &headers, hops)
                .unwrap()
                .contains_key(header::AUTHORIZATION)
        );
        let redirected = next_redirect_headers(&from, &other, &headers, hops).unwrap();
        assert!(!redirected.contains_key(header::AUTHORIZATION));
        assert_eq!(redirected[header::ACCEPT], headers[header::ACCEPT]);
    }
    assert_eq!(
        next_redirect_headers(&from, &other, &headers, 5)
            .unwrap_err()
            .code,
        Code::RedirectLimitExceeded
    );
    assert!(
        next_redirect_headers(
            &from,
            &Url::parse("https://example.org").unwrap(),
            &headers,
            0
        )
        .is_err()
    );

    let mut calls = 0;
    let disabled = automatic(AutoCheckPolicy::Off, || {
        calls += 1;
        std::future::ready(Ok(FetchResult::Modified {
            etag: None,
            body: (),
        }))
    })
    .await
    .unwrap();
    assert_eq!((disabled, calls), (FetchResult::Disabled, 0));
    assert_eq!(
        auto_check(
            AutoCheckPolicy::Off,
            &RepoSlug::parse("owner/repo").unwrap(),
            Some("\ninvalid")
        )
        .await
        .unwrap(),
        FetchResult::Disabled
    );
}

#[tokio::test]
async fn http_guard_bodies_and_status_without_network() {
    let chunk = vec![42; 1024 * 1024];
    let bytes = chunk.as_slice();
    for (limit, chunks) in [(MAX_JSON_BYTES, 2), (MAX_ARCHIVE_BYTES, 256)] {
        assert!(check_length(limit, limit).is_ok());
        assert_eq!(
            check_length(limit + 1, limit).unwrap_err().code,
            Code::ResponseTooLarge
        );
        let mut input = std::iter::repeat_n(bytes, chunks);
        read_bounded(&mut tokio::io::sink(), limit, async || Ok(input.next()))
            .await
            .unwrap();
        let mut input = std::iter::repeat_n(bytes, chunks).chain([&bytes[..1]]);
        assert_eq!(
            read_bounded(&mut tokio::io::sink(), limit, async || Ok(input.next()))
                .await
                .unwrap_err()
                .code,
            Code::ResponseTooLarge
        );
    }
    let mut headers = HeaderMap::new();
    headers.insert(header::ETAG, HeaderValue::from_static(r#"W/"release-v1""#));
    headers.insert("x-ratelimit-reset", HeaderValue::from_static("123456"));
    assert!(response_not_modified(StatusCode::NOT_MODIFIED, &headers).unwrap());
    assert_eq!(
        header_text(&headers, header::ETAG),
        Some(r#"W/"release-v1""#)
    );
    assert!(!response_not_modified(StatusCode::OK, &headers).unwrap());
    for status in [StatusCode::FORBIDDEN, StatusCode::TOO_MANY_REQUESTS] {
        let error = response_not_modified(status, &headers).unwrap_err();
        assert_eq!(error.code, Code::RateLimited);
        assert!(error.message().contains("123456"));
        assert!(!error.message().contains('='));
    }
}
