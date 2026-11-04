//! Pure helpers for the command line: parsing `[user@]host:path` and splitting paths into the root
//! and name the engine works with. Kept here, not in `main.rs`, so they are unit tested.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSpec {
    pub user: Option<String>,
    /// Hostname or IP, without brackets.
    pub host: String,
    /// As typed; empty means the login directory.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecError(&'static str);

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for SpecError {}

impl RemoteSpec {
    /// Parses `[user@]host:path` or `[user@][v6addr]:path`. The port is a separate option, so a
    /// colon is never ambiguous.
    pub fn parse(s: &str) -> Result<Self, SpecError> {
        // A '@' only separates the user when it comes before the host's ':' (paths may contain '@').
        let colon = s.find(':').ok_or(SpecError("expected [user@]host:path"))?;
        let (user, rest) = match s[..colon].rfind('@').or_else(|| bracket_user(s)) {
            Some(at) => (Some(&s[..at]), &s[at + 1..]),
            None => (None, s),
        };
        if let Some(u) = user
            && (u.is_empty() || u.starts_with('-') || u.chars().any(|c| c.is_control() || c == ':'))
        {
            return Err(SpecError("invalid user name"));
        }
        let (host, path) = if let Some(v6) = rest.strip_prefix('[') {
            let end = v6.find(']').ok_or(SpecError("unclosed '[' in host"))?;
            let path = v6[end + 1..]
                .strip_prefix(':')
                .ok_or(SpecError("expected ':' after ']'"))?;
            (&v6[..end], path)
        } else {
            rest.split_once(':')
                .ok_or(SpecError("expected [user@]host:path"))?
        };
        if host.is_empty() {
            return Err(SpecError("empty host"));
        }
        Ok(Self {
            user: user.map(str::to_owned),
            host: host.to_owned(),
            path: path.to_owned(),
        })
    }
}

/// `user@[v6]:path`: the first ':' is inside the brackets, so look for '@' before the '['.
fn bracket_user(s: &str) -> Option<usize> {
    let open = s.find("@[")?;
    (open < s.find(']')?).then_some(open)
}

/// Splits a slash path into (parent, name). `name` is empty when the path names a directory
/// (empty, ".", or a trailing slash): the caller then appends the source's name.
pub fn split_slash_path(path: &str) -> (String, String) {
    if path.is_empty() || path == "." || path.ends_with('/') {
        let dir = if path.is_empty() { "." } else { path };
        return (dir.to_owned(), String::new());
    }
    match path.rsplit_once('/') {
        Some(("", name)) => ("/".to_owned(), name.to_owned()),
        Some((parent, name)) => (parent.to_owned(), name.to_owned()),
        None => (".".to_owned(), path.to_owned()),
    }
}

/// Parses a byte size: a plain number, or one with a binary suffix K, M, G or T (optionally followed
/// by "iB" or "B"; case-insensitive). "10G" is 10 GiB.
pub fn parse_size(s: &str) -> Result<u64, SpecError> {
    let t = s.trim();
    let digits = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(digits);
    let n: u64 = num
        .parse()
        .map_err(|_| SpecError("expected a size like 500M or 10G"))?;
    let shift = match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 0,
        "k" | "kb" | "kib" => 10,
        "m" | "mb" | "mib" => 20,
        "g" | "gb" | "gib" => 30,
        "t" | "tb" | "tib" => 40,
        _ => return Err(SpecError("unknown size unit (use K, M, G or T)")),
    };
    n.checked_mul(1 << shift).ok_or(SpecError("size too large"))
}

/// `s` safe to print to a terminal: control characters (escape sequences, carriage returns,
/// newlines) are shown escaped, so text a server chose (an error message, a file name) cannot
/// rewrite the screen or fake output. Everything else is kept as is.
pub fn printable(s: &str) -> String {
    s.chars()
        .flat_map(|c| -> Box<dyn Iterator<Item = char>> {
            if c.is_control() {
                Box::new(c.escape_default())
            } else {
                Box::new(std::iter::once(c))
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {

    #[test]
    fn printable_escapes_control_characters_only() {
        assert_eq!(
            printable("plain é \"quoted\" path/x"),
            "plain é \"quoted\" path/x"
        );
        assert_eq!(
            printable("evil\u{1b}[2J\rok\nnext\u{7}"),
            "evil\\u{1b}[2J\\rok\\nnext\\u{7}"
        );
    }
    use super::*;

    fn spec(user: Option<&str>, host: &str, path: &str) -> RemoteSpec {
        RemoteSpec {
            user: user.map(str::to_owned),
            host: host.to_owned(),
            path: path.to_owned(),
        }
    }

    #[test]
    fn parses_common_forms() {
        let p = |s| RemoteSpec::parse(s).expect(s);
        assert_eq!(p("box:/data/x"), spec(None, "box", "/data/x"));
        assert_eq!(p("user@box:x"), spec(Some("user"), "box", "x"));
        assert_eq!(p("box:"), spec(None, "box", ""));
        assert_eq!(p("u@[::1]:/tmp/f"), spec(Some("u"), "::1", "/tmp/f"));
        assert_eq!(p("[fe80::1]:f"), spec(None, "fe80::1", "f"));
        assert_eq!(p("box:dir/a@b:c"), spec(None, "box", "dir/a@b:c"));
    }

    #[test]
    fn rejects_malformed_specs() {
        for s in [
            "nocolon",
            ":path",
            "@box:p",
            "-oProxy@box:p",
            "[::1/p",
            "[::1]p",
            "u@:p",
        ] {
            assert!(RemoteSpec::parse(s).is_err(), "{s} should be rejected");
        }
    }

    #[test]
    fn splits_paths() {
        let s = |p| split_slash_path(p);
        assert_eq!(s("dir/f"), ("dir".into(), "f".into()));
        assert_eq!(s("f"), (".".into(), "f".into()));
        assert_eq!(s("/f"), ("/".into(), "f".into()));
        assert_eq!(s("/a/b/"), ("/a/b/".into(), String::new()));
        assert_eq!(s(""), (".".into(), String::new()));
        assert_eq!(s("."), (".".into(), String::new()));
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("0"), Ok(0));
        assert_eq!(parse_size("512"), Ok(512));
        assert_eq!(parse_size("10G"), Ok(10 << 30));
        assert_eq!(parse_size("500mib"), Ok(500 << 20));
        assert_eq!(parse_size("2TB"), Ok(2 << 40));
        for bad in [
            "",
            "G",
            "1.5G",
            "-1",
            "10X",
            "99999999999999999999",
            "16777216T",
        ] {
            assert!(parse_size(bad).is_err(), "{bad} should be rejected");
        }
    }
}
