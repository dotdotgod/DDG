use crate::{
    config::{
        AutoCheckPolicy, DEFAULT_UPDATE_REPO, RepoSlug, SourceOrigin, UpdateChannel, UpdateConfig,
    },
    error::{UpdateError, UpdateErrorCode as Code},
    release::{build_release_web_url, sanitize_release_notes, validate_open_url},
    version::{ReleaseTag, compare_and_select_release, validate_apply_consent},
};

#[test]
fn url_and_input() {
    for invalid in [
        "",
        "owner",
        "owner/",
        "/repo",
        "a/.",
        "a/..",
        "https://a/b",
        "a/b?c",
        "a/b#c",
        "a/b c",
        "a/\0",
        "a/b/c",
    ] {
        assert!(RepoSlug::parse(invalid).is_err(), "{invalid:?}");
    }
    assert!(RepoSlug::parse(&format!("{}/repo", "a".repeat(40))).is_err());
    assert!(RepoSlug::parse(&format!("owner/{}", "b".repeat(101))).is_err());
    for invalid in [
        "v01.0.0",
        "1.0",
        "V1.0.0",
        "v1.0.0/x",
        "v1.0.0+..",
        "v1.0.0?",
        "v1.0.0#",
        "1.0.0 ",
        "1.0.0\n",
    ] {
        assert!(ReleaseTag::parse(invalid).is_err(), "{invalid:?}");
    }
    assert_eq!(
        RepoSlug::parse(DEFAULT_UPDATE_REPO).unwrap().as_str(),
        "dotdotgod/DDG"
    );
    let repo = RepoSlug::parse("Owner/my.repo").unwrap();
    let tag = ReleaseTag::parse("v1.0.0-rc.1+build.2").unwrap();
    let url = build_release_web_url(&repo, &tag);
    assert_eq!(
        url,
        "https://github.com/Owner/my.repo/releases/tag/v1.0.0-rc.1%2Bbuild.2"
    );
    assert!(validate_open_url(&url).is_ok());
}

#[test]
fn version_policy() {
    let version = |s| semver::Version::parse(s).unwrap();
    let tags = [
        ReleaseTag::parse("v2.0.0-rc.2").unwrap(),
        ReleaseTag::parse("v2.0.0-rc.10").unwrap(),
        ReleaseTag::parse("v1.9.0").unwrap(),
    ];
    let current = version("1.0.0");
    for (channel, expected) in [
        (UpdateChannel::Stable, &tags[2]),
        (UpdateChannel::Prerelease, &tags[1]),
    ] {
        assert_eq!(
            compare_and_select_release(&current, &tags, channel, false, |_| true).unwrap(),
            Some(expected)
        );
    }
    let current = version("2.0.0");
    assert_eq!(
        compare_and_select_release(&current, &tags, UpdateChannel::Prerelease, false, |_| true)
            .unwrap_err()
            .code,
        Code::DowngradeDenied
    );
    assert_eq!(
        compare_and_select_release(&current, &tags, UpdateChannel::Prerelease, true, |_| false)
            .unwrap_err()
            .code,
        Code::SchemaIncompatibleDowngrade
    );
    assert_eq!(
        compare_and_select_release(&current, &tags, UpdateChannel::Prerelease, true, |_| true)
            .unwrap(),
        Some(&tags[1])
    );
    assert_eq!(
        validate_apply_consent(&current, tags[1].version(), true, false, 0, false, false)
            .unwrap_err()
            .code,
        Code::DowngradeDenied
    );
    assert_eq!(
        validate_apply_consent(&current, tags[1].version(), false, true, 1, true, false)
            .unwrap_err()
            .code,
        Code::SchemaIncompatibleDowngrade
    );
    assert_eq!(
        validate_apply_consent(&current, tags[1].version(), true, true, 1, true, false)
            .unwrap_err()
            .code,
        Code::HolderRestartRequired
    );
    assert!(validate_apply_consent(&current, tags[1].version(), true, true, 1, true, true).is_ok());
    assert!(
        validate_apply_consent(&current, tags[1].version(), true, true, 0, true, false).is_ok()
    );
    let plus_build = [ReleaseTag::parse("2.0.0+build").unwrap()];
    assert_eq!(
        compare_and_select_release(&current, &plus_build, UpdateChannel::Stable, false, |_| {
            true
        })
        .unwrap(),
        None
    );
}

#[test]
fn release_notes() {
    let text = "\x1b[31mHello\x1b[0m\u{202e}\u{2028}\u{2029}\n\x1b]0;title\x07World";
    assert_eq!(sanitize_release_notes(text), "Hello\nWorld");
    let lines = format!("{}visible\nnot-visible", "\n".repeat(11));
    assert_eq!(
        sanitize_release_notes(&lines),
        format!("{}visible", "\n".repeat(11))
    );
    assert_eq!(
        sanitize_release_notes(&"한".repeat(1201)),
        "한".repeat(1200)
    );
    for invalid in [
        "http://github.com/a/b/releases/tag/v1.0.0",
        "file:///tmp/a",
        "smb://github.com/a",
        "https://evil.example/a/b/releases/tag/v1.0.0",
        "https://github.com/a/b/releases/tag/v1.0.0#foo",
        "https://api.github.com/a/b/releases/tag/v1.0.0",
        "https://github.com@evil.example/a/b/releases/tag/v1.0.0",
        "https://github.com/a/b/releases/tag/v1.0.0%2Fmore",
    ] {
        assert!(validate_open_url(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn release_notes_ignore_api_html_url() {
    let repo = RepoSlug::parse("owner/repo").unwrap();
    for html_url in [
        "file:///tmp/a",
        "smb://server/share",
        "https://example.org",
        "https://github.com/other/repo/releases/tag/v9.0.0",
    ] {
        let release: crate::release::GitHubRelease = serde_json::from_value(serde_json::json!({
            "tag_name": "v1.0.0", "draft": false, "prerelease": false, "html_url": html_url,
            "body": "\x1b[31mRelease notes\x1b[0m",
        }))
        .unwrap();
        assert_eq!(release.notes(), "Release notes");
        assert_eq!(
            release.web_url(&repo).unwrap(),
            "https://github.com/owner/repo/releases/tag/v1.0.0"
        );
    }
}

#[test]
fn config_and_redaction() {
    assert_eq!(Code::ALL.len(), 31);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("update.json");
    let mut config = UpdateConfig::load(&file).unwrap();
    assert_eq!(config.check_interval_secs, 21_600);
    config.auto_check = AutoCheckPolicy::Off;
    config.check_interval_secs = 1;
    config.repo = Some("owner/repo".into());
    config.save(&file).unwrap();
    let config = UpdateConfig::load(&file).unwrap();
    assert_eq!(config.check_interval_secs, 900);
    assert_eq!(
        config.resolve_repo(Some("override/repo")).unwrap().1,
        SourceOrigin::Env
    );
    assert_eq!(config.resolve_repo(None).unwrap().1, SourceOrigin::Config);
    assert!(
        !std::fs::read_to_string(file)
            .unwrap()
            .contains("skipped_version")
    );
    for secret in [
        "GITHUB_TOKEN=<token> GITHUB_TOKEN=secret_value",
        "Gh_ToKeN=secret_value",
        "ghp_secret_value",
        "gho_secret_value",
        "ghu_secret_value",
        "ghs_secret_value",
        "ghr_secret_value",
        "github_pat_secret_value",
        "Authorization: Basic secret_value",
        "Bearer secret_value",
        "https://user:secret_value@github.com",
        "ddg_session_token: secret_value",
        "ses_secret_value",
        "-----BEGIN PRIVATE KEY-----\nsecret_value",
    ] {
        let error = UpdateError::new(Code::NetworkError, secret);
        assert!(!error.message().contains("secret_value"), "{error:?}");
        assert!(
            !serde_json::to_string(&error)
                .unwrap()
                .contains("secret_value")
        );
    }
}
