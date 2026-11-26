/// Removes credentials before text crosses a logging or presentation boundary.
pub fn redact_sensitive_text(text: &str) -> String {
    const LINE_SECRETS: &[&str] = &[
        "github_token=",
        "gh_token=",
        "authorization:",
        "ddg_session_token",
    ];
    const TOKEN_PREFIXES: &[&str] = &[
        "bearer ",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "ses_",
    ];
    let lower = text.to_ascii_lowercase();
    let mut output = String::new();
    let mut i = 0;
    while i < text.len() {
        let remaining = &lower[i..];
        if remaining.starts_with("private key") {
            output.push_str("[redacted]");
            break; // PEM material can span multiple lines.
        }
        let skip = if LINE_SECRETS.iter().any(|s| remaining.starts_with(s)) {
            Some(remaining.find('\n').unwrap_or(remaining.len()))
        } else if let Some(prefix) = TOKEN_PREFIXES.iter().find(|s| remaining.starts_with(**s)) {
            let token = remaining[prefix.len()..].trim_start_matches([' ', '\t']);
            let len = token
                .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>'))
                .unwrap_or(token.len());
            Some(remaining.len() - token.len() + len)
        } else if let Some(authority) = remaining.strip_prefix("://") {
            let authority = authority.split(['/', '?', '#']).next().unwrap_or_default();
            authority.rfind('@').map(|at| {
                output.push_str("://");
                3 + at + 1
            })
        } else {
            None
        };
        if let Some(skip) = skip {
            output.push_str("[redacted]");
            i += skip;
        } else {
            let c = text[i..].chars().next().unwrap();
            output.push(c);
            i += c.len_utf8();
        }
    }
    output
}
