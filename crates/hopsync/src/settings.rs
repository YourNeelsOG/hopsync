//! The settings file a person edits: `/etc/hopsync/config.toml`, or
//! `~/.config/hopsync/config.toml` without root. `hopsync restart` and `hopsync setup` apply it.
//!
//! It is a small subset of TOML, `key = "value"` lines and comments, read strictly: this file decides
//! how much of the box the web UI reaches, so a typo stops with its line number and changes nothing.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The web UI reaches the whole box, or the whole home folder without root.
    pub full_access: bool,
    /// The folder used when `full_access` is off.
    pub folder: PathBuf,
}

pub fn parse(text: &str) -> Result<Settings, String> {
    let (mut access, mut folder) = (None, None);
    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, rest) = line
            .split_once('=')
            .ok_or_else(|| format!("line {n}: expected key = \"value\""))?;
        let rest = rest.trim();
        let value = rest
            .strip_prefix('"')
            .and_then(|r| r.split_once('"'))
            .filter(|(_, after)| after.trim().is_empty() || after.trim().starts_with('#'))
            .map(|(v, _)| v)
            .ok_or_else(|| format!("line {n}: the value needs double quotes, like \"folder\""))?;
        let slot = match key.trim() {
            "access" => &mut access,
            "folder" => &mut folder,
            other => {
                return Err(format!(
                    "line {n}: unknown setting {other:?}. The settings are access and folder."
                ));
            }
        };
        if slot.replace(value.to_owned()).is_some() {
            return Err(format!("line {n}: {} is set twice", key.trim()));
        }
    }
    let full_access = match access.as_deref() {
        None | Some("folder") => false,
        Some("full") => true,
        Some(other) => {
            return Err(format!(
                "access is {other:?}, and it must be \"folder\" or \"full\""
            ));
        }
    };
    let folder = folder.ok_or("folder is missing")?;
    Ok(Settings {
        full_access,
        folder: PathBuf::from(folder),
    })
}

pub fn render(s: &Settings) -> String {
    format!(
        "# Hopsync settings. Run `hopsync restart` after changing this file.\n\
         \n\
         # \"folder\": the web UI stays in the folder below. This is the safe choice.\n\
         # \"full\":   the web UI reads and writes the whole box as root, or your whole home folder\n\
         #           without root. Anyone with the web UI password then controls that much.\n\
         access = \"{}\"\n\
         \n\
         # The folder for your files when access is \"folder\". No spaces.\n\
         folder = \"{}\"\n",
        if s.full_access { "full" } else { "folder" },
        s.folder.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(full_access: bool) -> Settings {
        Settings {
            full_access,
            folder: PathBuf::from("/srv/hopsync"),
        }
    }

    #[test]
    fn what_setup_writes_reads_back() {
        for full in [false, true] {
            assert_eq!(parse(&render(&folder(full))).unwrap(), folder(full));
        }
    }

    #[test]
    fn a_hand_edited_file_with_comments_is_read() {
        let text = "# mine\n\n  access = \"full\"   # yes\nfolder=\"/srv/hopsync\"\n";
        assert_eq!(parse(text).unwrap(), folder(true));
    }

    #[test]
    fn access_defaults_to_the_safe_choice() {
        assert_eq!(parse("folder = \"/srv/hopsync\"").unwrap(), folder(false));
    }

    #[test]
    fn a_mistake_is_refused_and_says_where() {
        for (text, says) in [
            ("access = full\nfolder = \"/a\"", "line 1"),
            ("folder = \"/a\"\nacess = \"full\"", "line 2"),
            ("folder = \"/a\"\njunk", "line 2"),
            ("access = \"everything\"\nfolder = \"/a\"", "must be"),
            ("access = \"full\"", "folder is missing"),
            ("folder = \"/a\"\nfolder = \"/b\"", "set twice"),
            ("folder = \"/a\" trailing", "line 1"),
            ("access = \"FULL\"\nfolder = \"/a\"", "must be"),
        ] {
            let e = parse(text).unwrap_err();
            assert!(e.contains(says), "{text:?} gave {e}");
        }
    }
}
