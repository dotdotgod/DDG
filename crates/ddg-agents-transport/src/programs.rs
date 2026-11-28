//! Shared, read-only program icon mappings. Invalid files never replace built-ins.

use crate::profile::{HostsConfig, unsafe_text_char};
use ddg_agents_proto::id::sanitize_display_text;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{self, Read};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const PROGRAMS_SCHEMA_VERSION: u32 = 1;
pub const MAX_PROGRAMS_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramsConfig {
    pub schema_version: u32,
    pub programs: Vec<ProgramDefinition>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramDefinition {
    pub id: String,
    pub label: String,
    pub icon: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub tint: bool,
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub shell: bool,
    #[serde(default)]
    pub matchers: ProgramMatchers,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProgramMatchers {
    pub osc_apps: Vec<String>,
    pub comms: Vec<String>,
    pub script_hints: Vec<String>,
    pub agent_profiles: Vec<String>,
}

impl Default for ProgramsConfig {
    fn default() -> Self {
        serde_json::from_str(include_str!("builtin_programs.json"))
            .expect("invalid built-in program catalog")
    }
}

pub fn normalize_process_name(raw: &str) -> Option<String> {
    let name = raw.rsplit(['/', '\\']).next()?;
    let name = name.strip_prefix('-').unwrap_or(name);
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    let name = if ext.eq_ignore_ascii_case("exe") {
        stem
    } else {
        name
    };
    let text = sanitize_display_text(name, 64);
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

pub fn strip_script_extension(name: &str) -> &str {
    if let Some((stem, ext)) = name.rsplit_once('.')
        && !stem.is_empty()
        && ["js", "mjs", "cjs", "ts", "mts", "cts", "py", "rb"]
            .iter()
            .any(|e| ext.eq_ignore_ascii_case(e))
    {
        return stem;
    }
    name
}

pub fn is_interpreter_name(name: &str) -> bool {
    [
        "node", "nodejs", "bun", "bunx", "deno", "python", "python3", "npx", "pnpm", "yarn", "uv",
        "uvx", "ruby", "bundle", "perl", "tsx", "env", "sudo", "nice", "time", "watch", "nodemon",
    ]
    .iter()
    .any(|interp| name.eq_ignore_ascii_case(interp))
}

impl ProgramsConfig {
    pub fn find_by_token(
        &self,
        token: &str,
        aliases: impl Fn(&ProgramMatchers) -> &[String],
    ) -> Option<&ProgramDefinition> {
        let generic = |p: &ProgramDefinition| {
            ["agent", "program", "shell"]
                .iter()
                .any(|key| p.id.eq_ignore_ascii_case(key))
        };
        let id = |p: &ProgramDefinition| p.id.eq_ignore_ascii_case(token);
        let alias = |p: &ProgramDefinition| {
            aliases(&p.matchers)
                .iter()
                .any(|key| key.eq_ignore_ascii_case(token))
        };
        self.programs
            .iter()
            .find(|p| !generic(p) && id(p))
            .or_else(|| self.programs.iter().find(|p| !generic(p) && alias(p)))
            .or_else(|| self.programs.iter().find(|p| generic(p) && id(p)))
            .or_else(|| self.programs.iter().find(|p| generic(p) && alias(p)))
    }

    pub fn default_path() -> io::Result<PathBuf> {
        Ok(HostsConfig::default_path()?.with_file_name("programs.json"))
    }

    pub fn load(path: &Path) -> Self {
        match Self::read(path) {
            Ok(config) => config,
            Err(error) => {
                eprintln!("warning: programs.json: {error}; using built-in program mappings");
                Self::default()
            }
        }
    }

    fn read(path: &Path) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NONBLOCK);
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("not a regular file"));
        }
        let mut bytes = Vec::new();
        file.take((MAX_PROGRAMS_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_PROGRAMS_BYTES {
            return Err(io::Error::other("file exceeds 256 KiB"));
        }
        let config: Self =
            serde_json::from_slice(&bytes).map_err(|_| io::Error::other("invalid structure"))?;
        if config.schema_version != PROGRAMS_SCHEMA_VERSION {
            return Err(io::Error::other("unsupported schema_version"));
        }
        for program in &config.programs {
            if !(1..=64).contains(&program.icon.len())
                || !program
                    .icon
                    .bytes()
                    .all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
            {
                return Err(io::Error::other("invalid icon key"));
            }
            if program.command.as_ref().is_some_and(|command| {
                !(1..=64).contains(&command.chars().count())
                    || command.chars().any(unsafe_text_char)
            }) {
                return Err(io::Error::other("invalid command"));
            }
            if program.color.as_ref().is_some_and(|color| {
                let bytes = color.as_bytes();
                bytes.len() != 7
                    || bytes[0] != b'#'
                    || !bytes[1..].iter().all(u8::is_ascii_hexdigit)
            }) {
                return Err(io::Error::other("invalid color"));
            }
            let ProgramMatchers {
                osc_apps,
                comms,
                script_hints,
                agent_profiles,
            } = &program.matchers;
            for value in osc_apps
                .iter()
                .chain(comms)
                .chain(script_hints)
                .chain(agent_profiles)
            {
                if value.chars().count() > 128 || value.chars().any(unsafe_text_char) {
                    return Err(io::Error::other("invalid matcher"));
                }
            }
        }
        Ok(config)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::agent::tests::TempDir;
    use crate::profile::split_safe_tokens;
    use std::fs;

    #[test]
    fn builtin_catalog_and_normalization() {
        let defaults = ProgramsConfig::default();
        let tmp = TempDir::new();
        let path = tmp.0.join("programs.json");
        fs::write(&path, include_str!("builtin_programs.json")).unwrap();
        assert_eq!(ProgramsConfig::read(&path).unwrap(), defaults);
        let ids: std::collections::HashSet<_> = defaults.programs.iter().map(|p| &p.id).collect();
        assert_eq!(ids.len(), 128);
        assert_eq!(ids.len(), defaults.programs.len());
        for (token, id) in [
            ("git", "git"),
            ("docker", "docker"),
            ("htop", "htop"),
            ("vim", "vim"),
            ("nodejs", "node"),
            ("python3", "python"),
            ("bun", "bun"),
            ("go", "go"),
            ("ssh", "ssh"),
            ("tmux", "tmux"),
            ("agent", "cursor"),
            ("program", "program"),
            ("pwsh", "shell"),
            ("claude", "claude-code"),
            ("R", "r"),
            ("npx", "npm"),
            ("tsx", "tsc"),
        ] {
            assert_eq!(defaults.find_by_token(token, |m| &m.comms).unwrap().id, id);
        }
        for (raw, expected) in [
            ("/bin/agy", "agy"),
            ("-zsh", "zsh"),
            (r"C:\bin\codex.ExE", "codex"),
            ("  a\u{202e}gy ", "agy"),
        ] {
            assert_eq!(normalize_process_name(raw).as_deref(), Some(expected));
        }
        for (raw, expected) in [
            ("vite.js", "vite"),
            ("codex.MJS", "codex"),
            ("app.py", "app"),
            ("name.MTS", "name"),
            ("name.cts", "name"),
            (".js", ".js"),
        ] {
            assert_eq!(strip_script_extension(raw), expected);
        }
        assert!(is_interpreter_name("NodeJS"));
        assert!(!is_interpreter_name("zsh"));
    }

    #[test]
    fn programs_and_split_tokens() {
        let tmp = TempDir::new();
        let path = tmp.0.join("programs.json");
        let defaults = ProgramsConfig::default();
        for expected in [
            "claude-code",
            "codex",
            "pi",
            "gemini",
            "aider",
            "opencode",
            "cursor",
            "nvim",
            "cargo",
            "zsh",
            "bash",
            "fish",
        ] {
            assert!(
                defaults.programs.iter().any(|p| p.id == expected),
                "{expected}"
            );
        }
        let antigravity = defaults
            .programs
            .iter()
            .find(|p| p.id == "antigravity")
            .unwrap();
        assert_eq!(antigravity.label, "Antigravity");
        assert_eq!(antigravity.icon, "antigravity");
        let m = &antigravity.matchers;
        for aliases in [&m.osc_apps, &m.comms, &m.script_hints, &m.agent_profiles] {
            assert_eq!(aliases, &["antigravity", "agy"]);
        }
        assert_eq!(ProgramsConfig::load(&path), defaults);
        assert!(!path.exists());
        let valid = r#"{"schema_version":1,"programs":[{"id":"x","label":"X","icon":"x_1","matchers":{"comms":["x"]}}]}"#;
        fs::write(&path, valid).unwrap();
        let config = ProgramsConfig::load(&path);
        assert_eq!(config.programs.len(), 1);
        assert_eq!(config.programs[0].matchers.comms, ["x"]);
        assert!(!config.programs[0].tint);
        assert!(!config.programs[0].shell);
        assert_eq!(config.programs[0].color, None);
        assert_eq!(config.programs[0].command, None);
        for broken in [
            valid.replace("\"icon\":", "\"color\":\"red\",\"icon\":"),
            valid.replace("\"icon\":", "\"color\":\"#12345\",\"icon\":"),
            valid.replace("\"icon\":", "\"command\":\"\",\"icon\":"),
            valid.replace("\"icon\":", "\"command\":\"a\\u001bb\",\"icon\":"),
            "{broken".to_owned(),
            valid.replace("\"schema_version\":1", "\"schema_version\":2"),
            valid.replace("x_1", "../x"),
            valid.replace("[\"x\"]", "[\"x\\u001b\"]"),
            valid.replace("[\"x\"]", &format!("[\"{}\"]", "x".repeat(129))),
            format!("{valid}{}", " ".repeat(MAX_PROGRAMS_BYTES)),
        ] {
            fs::write(&path, &broken).unwrap();
            assert_eq!(ProgramsConfig::load(&path), defaults);
            assert_eq!(fs::read_to_string(&path).unwrap(), broken);
        }
        assert_eq!(
            split_safe_tokens("pi\t\"hi there\" 'literal $x' a\\ b ''").unwrap(),
            ["pi", "hi there", "literal $x", "a b", ""]
        );
        for ch in [
            '\n', '\r', '\0', '\x1b', '\x7f', '\u{0085}', '\u{2028}', '\u{2029}', '\u{061c}',
            '\u{200e}', '\u{200f}',
        ]
        .into_iter()
        .chain('\u{202a}'..='\u{202e}')
        .chain('\u{2066}'..='\u{2069}')
        {
            for raw in [
                format!("pi{ch}other"),
                format!("pi '{ch}'"),
                format!("pi \"{ch}\""),
                format!("pi \\{ch}"),
                format!("pi \"\\{ch}\""),
            ] {
                assert!(split_safe_tokens(&raw).is_err(), "{raw:?}");
            }
        }
        for raw in ["pi '", "pi \"", "pi \\", "pi \"\\"] {
            assert!(split_safe_tokens(raw).is_err(), "{raw:?}");
        }
    }
}
