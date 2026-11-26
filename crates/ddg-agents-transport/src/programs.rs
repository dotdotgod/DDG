//! Shared, read-only program icon mappings. Invalid files never replace built-ins.

use crate::profile::{HostsConfig, unsafe_text_char};
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
        let programs = [
            ("claude-code", "Claude Code", &["claude", "claude-code"][..]),
            ("codex", "Codex", &["codex"]),
            ("pi", "Pi", &["pi"]),
            ("gemini", "Gemini", &["gemini"]),
            ("aider", "Aider", &["aider"]),
            ("opencode", "OpenCode", &["opencode"]),
            ("cursor", "Cursor", &["cursor", "cursor-agent", "agent"]),
            ("nvim", "Neovim", &["nvim"]),
            ("cargo", "Cargo", &["cargo"]),
            ("zsh", "Zsh", &["zsh"]),
            ("bash", "Bash", &["bash"]),
            ("fish", "Fish", &["fish"]),
        ]
        .into_iter()
        .map(|(id, label, names)| {
            let names: Vec<_> = names.iter().map(|&name| name.to_owned()).collect();
            ProgramDefinition {
                id: id.into(),
                label: label.into(),
                icon: id.into(),
                matchers: ProgramMatchers {
                    osc_apps: names.clone(),
                    comms: names.clone(),
                    script_hints: names.clone(),
                    agent_profiles: names,
                },
            }
        })
        .collect();
        Self {
            schema_version: PROGRAMS_SCHEMA_VERSION,
            programs,
        }
    }
}

impl ProgramsConfig {
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
        assert_eq!(ProgramsConfig::load(&path), defaults);
        assert!(!path.exists());
        let valid = r#"{"schema_version":1,"programs":[{"id":"x","label":"X","icon":"x_1","matchers":{"comms":["x"]}}]}"#;
        fs::write(&path, valid).unwrap();
        let config = ProgramsConfig::load(&path);
        assert_eq!(config.programs.len(), 1);
        assert_eq!(config.programs[0].matchers.comms, ["x"]);
        for broken in [
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
