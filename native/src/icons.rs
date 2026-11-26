use gpui::{AssetSource, Hsla, Result, SharedString, Styled, Svg, px, svg};
use std::borrow::Cow;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
#[cfg(unix)]
use std::{
    fs::OpenOptions,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
};

use std::{
    collections::{HashMap, HashSet},
    fs::{File, Metadata},
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

#[derive(Clone, Default)]
pub struct AppAssets {
    custom: Arc<RwLock<CustomAssets>>,
}

#[derive(Default)]
struct CustomAssets {
    bytes: HashMap<String, Vec<u8>>,
    warned: HashSet<String>,
    resolved: HashMap<PathBuf, (IconStamp, Option<SharedString>)>,
}

fn program_asset(key: &str) -> Option<(&'static str, &'static [u8])> {
    Some(match key {
        "claude-code" => (
            "programs/claude-code.svg",
            include_bytes!("../assets/icons/claude-code.svg"),
        ),
        "codex" => (
            "programs/codex.svg",
            include_bytes!("../assets/icons/codex.svg"),
        ),
        "pi" => ("programs/pi.svg", include_bytes!("../assets/icons/pi.svg")),
        "gemini" => (
            "programs/gemini.svg",
            include_bytes!("../assets/icons/gemini.svg"),
        ),
        "aider" => (
            "programs/aider.svg",
            include_bytes!("../assets/icons/aider.svg"),
        ),
        "opencode" => (
            "programs/opencode.svg",
            include_bytes!("../assets/icons/opencode.svg"),
        ),
        "cursor" => (
            "programs/cursor.svg",
            include_bytes!("../assets/icons/cursor.svg"),
        ),
        "nvim" => (
            "programs/nvim.svg",
            include_bytes!("../assets/icons/nvim.svg"),
        ),
        "cargo" => (
            "programs/cargo.svg",
            include_bytes!("../assets/icons/cargo.svg"),
        ),
        "zsh" => (
            "programs/zsh.svg",
            include_bytes!("../assets/icons/zsh.svg"),
        ),
        "bash" => (
            "programs/bash.svg",
            include_bytes!("../assets/icons/bash.svg"),
        ),
        "fish" => (
            "programs/fish.svg",
            include_bytes!("../assets/icons/fish.svg"),
        ),
        _ => return None,
    })
}

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path.starts_with("custom/") {
            return Ok(self
                .custom
                .read()
                .unwrap()
                .bytes
                .get(path)
                .cloned()
                .map(Cow::Owned));
        }
        if let Some(key) = path
            .strip_prefix("programs/")
            .and_then(|p| p.strip_suffix(".svg"))
        {
            return Ok(program_asset(key).map(|(_, bytes)| Cow::Borrowed(bytes)));
        }
        Ok(Icon::ALL
            .iter()
            .map(|icon| icon.asset())
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(Icon::ALL
            .iter()
            .map(|icon| icon.path())
            .filter(|name| name.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}

#[derive(Clone, Copy)]
pub enum Icon {
    ChevronDown,
    ChevronRight,
    Pencil,
    Plus,
    X,
    Server,
    Terminal,
    Pin,
    Settings,
    Spinner,
}

impl Icon {
    pub const ALL: &'static [Self] = &[
        Self::ChevronDown,
        Self::ChevronRight,
        Self::Pencil,
        Self::Plus,
        Self::X,
        Self::Server,
        Self::Terminal,
        Self::Pin,
        Self::Settings,
        Self::Spinner,
    ];

    pub fn path(self) -> &'static str {
        self.asset().0
    }

    fn asset(self) -> (&'static str, &'static [u8]) {
        match self {
            Self::ChevronDown => (
                "icons/chevron-down.svg",
                include_bytes!("../../assets/icons/chevron-down.svg"),
            ),
            Self::ChevronRight => (
                "icons/chevron-right.svg",
                include_bytes!("../../assets/icons/chevron-right.svg"),
            ),
            Self::Pencil => (
                "icons/pencil.svg",
                include_bytes!("../../assets/icons/pencil.svg"),
            ),
            Self::Plus => (
                "icons/plus.svg",
                include_bytes!("../../assets/icons/plus.svg"),
            ),
            Self::X => ("icons/x.svg", include_bytes!("../../assets/icons/x.svg")),
            Self::Terminal => (
                "icons/terminal.svg",
                include_bytes!("../assets/icons/terminal.svg"),
            ),
            Self::Pin => ("icons/pin.svg", include_bytes!("../assets/icons/pin.svg")),
            Self::Settings => (
                "icons/settings.svg",
                include_bytes!("../assets/icons/settings.svg"),
            ),
            Self::Spinner => (
                "icons/spinner.svg",
                include_bytes!("../assets/icons/spinner.svg"),
            ),
            Self::Server => (
                "icons/server.svg",
                include_bytes!("../../assets/icons/server.svg"),
            ),
        }
    }
}

pub fn icon(kind: Icon, size_px: f32, color: impl Into<Hsla>) -> Svg {
    // GPUI 0.2.2 paints SVG masks only when the SVG itself has a text color.
    svg()
        .path(kind.path())
        .size(px(size_px))
        .flex_none()
        .text_color(color)
}

const MAX_ICON_BYTES: usize = 128 * 1024;
type IconStamp = (u64, u64, u64, i64, i64, i64, i64);

#[cfg(windows)]
fn icon_stamp(metadata: &Metadata) -> IconStamp {
    (
        0,
        0,
        metadata.file_size(),
        metadata.last_write_time() as i64,
        0,
        metadata.creation_time() as i64,
        0,
    )
}

#[cfg(unix)]
fn icon_stamp(metadata: &Metadata) -> IconStamp {
    (
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

impl AppAssets {
    pub fn resolve_icon_key(
        &self,
        dir: Option<&Path>,
        name: &str,
        is_remote: bool,
    ) -> SharedString {
        if !is_remote
            && let Some(dir) = dir
            && (name.ends_with(".svg") || name.ends_with(".png"))
        {
            if let Some(key) = self.custom_icon_key(dir, name) {
                return key;
            }
            if self.custom.write().unwrap().warned.insert(name.into()) {
                eprintln!("warning: unsafe or unreadable local icon; using the terminal icon");
            }
        }
        program_asset(name)
            .map_or(Icon::Terminal.path(), |(path, _)| path)
            .into()
    }

    fn custom_icon_key(&self, dir: &Path, name: &str) -> Option<SharedString> {
        let path = custom_path(dir, name)?;
        // In the warm render path, reuse bytes validated on the previous file descriptor.
        let observed = path.symlink_metadata().ok().filter(Metadata::is_file)?;
        if let Some((stamp, key)) = self.custom.read().unwrap().resolved.get(&path)
            && *stamp == icon_stamp(&observed)
        {
            return key.clone();
        }
        let (file, stamp) = open_custom_icon(&path)?;
        let bytes = load_custom_icon(file, name);
        let mut assets = self.custom.write().unwrap();
        let key = bytes.map(|bytes| {
            let hash = bytes.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
            });
            let key = format!("custom/{name}#{hash:016x}");
            assets.bytes.entry(key.clone()).or_insert(bytes);
            SharedString::from(key)
        });
        assets.resolved.insert(path, (stamp, key.clone()));
        key
    }
}

fn custom_path(dir: &Path, name: &str) -> Option<PathBuf> {
    if name.contains("..")
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return None;
    }
    Some(dir.join(name))
}

fn open_custom_icon(path: &Path) -> Option<(File, IconStamp)> {
    #[cfg(windows)]
    let file = ddg_agents_os::fs::open_regular(path).ok()?;
    #[cfg(unix)]
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    meta.is_file().then(|| (file, icon_stamp(&meta)))
}

fn load_custom_icon(file: File, name: &str) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take((MAX_ICON_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_ICON_BYTES {
        return None;
    }
    let valid = if name.ends_with(".svg") {
        valid_svg(std::str::from_utf8(&bytes).ok()?)
    } else if name.ends_with(".png") {
        bytes.starts_with(b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR")
            && [16, 20].into_iter().all(|start| {
                bytes.get(start..start + 4).is_some_and(|dim| {
                    (1..=512).contains(&u32::from_be_bytes(dim.try_into().unwrap()))
                })
            })
    } else {
        false
    };
    valid.then_some(bytes)
}

fn valid_svg(svg: &str) -> bool {
    let Ok(doc) = roxmltree::Document::parse(svg) else {
        return false;
    };
    if !doc.root_element().has_tag_name("svg") || doc.descendants().count() > 1024 {
        return false;
    }
    let dimension = |s: &str| {
        s.trim()
            .strip_suffix("px")
            .unwrap_or(s.trim())
            .parse::<f64>()
            .ok()
            .is_some_and(|n| n > 0. && n <= 512.)
    };
    let mut expanded = doc.descendants().count();
    for node in doc.descendants() {
        if node.is_pi() {
            return false;
        }
        if !node.is_element() {
            continue;
        }
        let tag = node.tag_name();
        if !matches!(tag.namespace(), None | Some("http://www.w3.org/2000/svg"))
            || node.ancestors().filter(|n| n.is_element()).count() > 16
            || !matches!(
                tag.name(),
                "svg"
                    | "g"
                    | "defs"
                    | "path"
                    | "rect"
                    | "circle"
                    | "ellipse"
                    | "line"
                    | "polyline"
                    | "polygon"
                    | "use"
                    | "title"
                    | "desc"
            )
        {
            return false;
        }
        for attr in node.attributes() {
            let value = attr.value();
            if attr.name() == "href"
                && tag.name() == "use"
                && matches!(
                    attr.namespace(),
                    None | Some("http://www.w3.org/1999/xlink")
                )
            {
                let Some(id) = value.strip_prefix('#').filter(|id| {
                    !id.is_empty()
                        && id
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
                }) else {
                    return false;
                };
                let mut targets = doc.descendants().filter(|n| n.attribute("id") == Some(id));
                let Some(target) = targets.next() else {
                    return false;
                };
                // Bounding a single expansion is insufficient if the same subtree is reused.
                if targets.next().is_some()
                    || target
                        .descendants()
                        .any(|n| n.has_tag_name("use") || n.has_tag_name("svg"))
                {
                    return false;
                }
                expanded += target.descendants().count();
                if expanded > 1024 {
                    return false;
                }
                continue;
            }
            if attr.namespace().is_some()
                || value.contains([':', '\\'])
                || value.contains('(') && attr.name() != "transform"
            {
                return false;
            }
            let valid = match attr.name() {
                "width" | "height" => dimension(value),
                "viewBox" => {
                    let values: Vec<_> = value
                        .split(|c: char| c.is_ascii_whitespace() || c == ',')
                        .filter(|v| !v.is_empty())
                        .collect();
                    values.len() == 4
                        && values[..2]
                            .iter()
                            .all(|v| v.parse::<f64>().is_ok_and(f64::is_finite))
                        && values[2..].iter().all(|v| dimension(v))
                }
                "transform"
                | "version"
                | "preserveAspectRatio"
                | "id"
                | "d"
                | "points"
                | "fill"
                | "fill-rule"
                | "stroke"
                | "stroke-width"
                | "stroke-linecap"
                | "stroke-linejoin"
                | "stroke-dasharray"
                | "stroke-opacity"
                | "fill-opacity"
                | "opacity"
                | "x"
                | "y"
                | "cx"
                | "cy"
                | "r"
                | "rx"
                | "ry"
                | "x1"
                | "x2"
                | "y1"
                | "y2" => true,
                _ => false,
            };
            if !valid {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_icons_are_listed_and_have_svg_mask_attributes() {
        let expected: Vec<_> = Icon::ALL
            .iter()
            .map(|icon| SharedString::from(icon.path()))
            .collect();
        let assets = AppAssets::default();
        assert_eq!(assets.list("icons/").unwrap(), expected);

        for kind in Icon::ALL {
            let path = kind.path();
            let bytes = assets.load(path).unwrap().expect(path);
            let svg = std::str::from_utf8(&bytes).expect(path);
            let (root, shapes) = svg
                .strip_prefix("<svg ")
                .and_then(|svg| svg.split_once('>'))
                .expect(path);
            for attribute in [
                r#"xmlns="http://www.w3.org/2000/svg""#,
                r#"width="24""#,
                r#"height="24""#,
                r#"viewBox="0 0 24 24""#,
                r#"fill="none""#,
                r##"stroke="#000""##,
                r#"stroke-width="2""#,
                r#"stroke-linecap="round""#,
                r#"stroke-linejoin="round""#,
            ] {
                assert!(root.contains(attribute), "{path}: missing {attribute}");
            }
            let (shapes, _) = shapes.split_once("</svg>").expect(path);
            assert!(
                shapes.trim().starts_with(if matches!(kind, Icon::Server) {
                    "<rect "
                } else {
                    "<path "
                }),
                "{path}: missing glyph"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn custom_icon_limits_and_remote_isolation() {
        let dir = std::env::temp_dir().join(format!("ddg-icons-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("test.svg");
        let assets = AppAssets::default();
        let svg = r##"<svg version="1.1" viewBox="0 0 512 512"><defs><path id="p" d="M0 0"/></defs><use href="#p"/></svg>"##;
        let padded = svg.to_owned() + &" ".repeat(MAX_ICON_BYTES - svg.len());
        std::fs::write(&path, &padded).unwrap();
        let key = assets.resolve_icon_key(Some(&dir), "test.svg", false);
        assert!(key.starts_with("custom/test.svg#"));
        assert_eq!(assets.load(&key).unwrap().unwrap().len(), MAX_ICON_BYTES);
        assert_eq!(
            assets.resolve_icon_key(Some(&dir), "test.svg", true),
            Icon::Terminal.path()
        );
        std::fs::write(&path, svg).unwrap();
        assert_ne!(assets.resolve_icon_key(Some(&dir), "test.svg", false), key);
        std::fs::write(&path, padded + " ").unwrap();
        assert!(assets.custom_icon_key(&dir, "test.svg").is_none());
        std::os::unix::fs::symlink(&path, dir.join("link.svg")).unwrap();
        std::fs::create_dir(dir.join("dir.svg")).unwrap();
        for name in ["link.svg", "dir.svg", "../test.svg"] {
            assert!(assets.custom_icon_key(&dir, name).is_none());
        }
        std::fs::remove_dir_all(dir).unwrap();
        for svg in [
            r#"<svg width="513"/>"#,
            r#"<svg viewBox="0 0 1024 1024"/>"#,
            "<svg><script/></svg>",
            "<svg><SCRIPT/></svg>",
            r#"<svg xmlns:s="http://www.w3.org/2000/svg"><s:script/></svg>"#,
            "<svg><foreignObject/></svg>",
            "<svg><style>@import url(x)</style></svg>",
            r#"<svg><image href="file:///etc/passwd"/></svg>"#,
            r#"<svg><image href="data:image/png;base64,AAAA"/></svg>"#,
            r#"<svg onload="x"/>"#,
            r#"<svg xmlns:xlink="http://www.w3.org/1999/xlink"><use xlink:href="https://example.org/x"/></svg>"#,
            r##"<svg><g id="a"><use href="#a"/></g></svg>"##,
            "<!DOCTYPE svg [<!ENTITY a 'b'>]><svg>&a;</svg>",
            &format!("<svg>{}</svg>", "<path/>".repeat(1025)),
            &format!("<svg>{}{}</svg>", "<g>".repeat(16), "</g>".repeat(16)),
        ] {
            assert!(!valid_svg(svg), "{svg}");
        }
    }

    #[test]
    fn unknown_assets_are_not_found() {
        let assets = AppAssets::default();
        assert!(assets.load("icons/unknown.svg").unwrap().is_none());
        assert!(assets.list("unknown/").unwrap().is_empty());
    }
}
