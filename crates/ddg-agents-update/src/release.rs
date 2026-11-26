use crate::{UpdateError, UpdateErrorCode as Code, config::RepoSlug, version::ReleaseTag};
use ddg_agents_proto::id::sanitize_display_text;

/// The API's html_url is deliberately not deserialized.
#[derive(Debug, serde::Deserialize)]
pub struct GitHubRelease {
    tag_name: String,
    body: Option<String>,
    pub draft: bool,
    pub prerelease: bool,
}

impl GitHubRelease {
    pub fn tag(&self) -> Result<ReleaseTag, UpdateError> {
        ReleaseTag::parse(&self.tag_name)
    }

    pub fn notes(&self) -> String {
        sanitize_release_notes(self.body.as_deref().unwrap_or_default())
    }

    pub fn web_url(&self, repo: &RepoSlug) -> Result<String, UpdateError> {
        Ok(build_release_web_url(repo, &self.tag()?))
    }
}

pub fn sanitize_release_notes(notes: &str) -> String {
    let mut result = String::new();
    let mut remaining = 1_200;
    for (i, line) in notes.lines().take(12).enumerate() {
        if remaining == 0 {
            break;
        }
        if i != 0 {
            result.push('\n');
            remaining -= 1;
        }
        let line = strip_ansi(line).replace(['\u{2028}', '\u{2029}'], "");
        let line = sanitize_display_text(&line, remaining);
        remaining -= line.chars().count();
        result.push_str(&line);
    }
    result
}

fn strip_ansi(line: &str) -> String {
    fn control_string(chars: &mut impl Iterator<Item = char>) {
        let mut esc = false;
        for c in chars {
            if matches!(c, '\x07' | '\u{009c}') || (esc && c == '\\') {
                break;
            }
            esc = c == '\x1b';
        }
    }

    let mut result = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        let command = match c {
            '\x1b' => chars.next().unwrap_or_default(),
            '\u{009b}' => '[',
            '\u{0090}' | '\u{0098}' | '\u{009d}'..='\u{009f}' => ']',
            _ => {
                result.push(c);
                continue;
            }
        };
        match command {
            '[' => {
                chars.by_ref().find(|&c| ('\x40'..='\x7e').contains(&c));
            }
            ']' | 'P' | 'X' | '^' | '_' => control_string(&mut chars),
            '\x20'..='\x2f' => {
                chars.by_ref().find(|&c| !('\x20'..='\x2f').contains(&c));
            }
            _ => {}
        }
    }
    result
}

pub fn build_release_web_url(repo: &RepoSlug, tag: &ReleaseTag) -> String {
    // '+' is the only non-unreserved character in a validated SemVer tag.
    format!(
        "https://github.com/{}/releases/tag/{}",
        repo.as_str(),
        tag.as_str().replace('+', "%2B")
    )
}

/// Accept only the exact shape emitted by the builder, not GitHub API html_url.
pub fn validate_open_url(url: &str) -> Result<(), UpdateError> {
    if let Some(path) = url.strip_prefix("https://github.com/")
        && let Some((repo, tag)) = path.split_once("/releases/tag/")
        && let Ok(repo) = RepoSlug::parse(repo)
        && let Ok(tag) = ReleaseTag::parse(&tag.replace("%2B", "+"))
        && build_release_web_url(&repo, &tag) == url
    {
        return Ok(());
    }
    Err(UpdateError::new(
        Code::DisallowedUrlOrHost,
        "Not a canonical GitHub release URL",
    ))
}
