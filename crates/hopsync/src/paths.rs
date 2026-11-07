//! Validated path types. Every path the engine touches is built from these, so a hostile remote
//! listing or a mistyped argument cannot point outside the chosen roots (plan.md §7).
//!
//! - `Name`: one path component, as received from a listing or user input.
//! - `RelPath`: a sequence of `Name`s, relative to a root; joining it under a root cannot escape it.
//! - `RemoteRoot`: the remote base directory the user chose, normalized.
//!
//! Names are UTF-8 because the SFTP client exposes paths as `String`; non-UTF-8 remote names cannot be
//! represented and are reported as unsupported by the listing code.

use std::fmt;

mod local;
pub use local::{DirEntry, EntryKind, Hidden, LocalRoot, OpenMode};

/// Linux NAME_MAX. Longer names cannot be created locally on common filesystems.
pub const NAME_MAX_BYTES: usize = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathError {
    Empty,
    DotOrDotDot,
    ContainsSlash,
    ContainsNul,
    /// C0/C1 control characters, including newline and tab. Rejected rather than escaped because such
    /// names break logs, terminals and the UI, and plan.md §7 rejects them from listings.
    ContainsControl,
    TooLong,
    /// A `..` component in a root or relative path.
    ParentComponent,
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "empty name",
            Self::DotOrDotDot => "name is \".\" or \"..\"",
            Self::ContainsSlash => "name contains '/'",
            Self::ContainsNul => "name contains a NUL byte",
            Self::ContainsControl => "name contains a control character",
            Self::TooLong => "name is longer than 255 bytes",
            Self::ParentComponent => "path contains a \"..\" component",
        })
    }
}

impl std::error::Error for PathError {}

/// One path component.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Name(String);

impl Name {
    pub fn new(s: &str) -> Result<Self, PathError> {
        if s.is_empty() {
            return Err(PathError::Empty);
        }
        if s == "." || s == ".." {
            return Err(PathError::DotOrDotDot);
        }
        // NUL is also a control character; checked first so the error names the sharper problem.
        if s.contains('\0') {
            return Err(PathError::ContainsNul);
        }
        if s.contains('/') {
            return Err(PathError::ContainsSlash);
        }
        if s.chars().any(char::is_control) {
            return Err(PathError::ContainsControl);
        }
        if s.len() > NAME_MAX_BYTES {
            return Err(PathError::TooLong);
        }
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A relative path made of validated components. May be empty (the root itself).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelPath(Vec<Name>);

impl RelPath {
    /// Parses "a/b/c". Empty components, `.` and `..` are errors, not normalized away: a relative
    /// path from a listing or a journal that needs normalizing is a bug or an attack.
    pub fn parse(s: &str) -> Result<Self, PathError> {
        if s.is_empty() {
            return Ok(Self::default());
        }
        s.split('/')
            .map(Name::new)
            .collect::<Result<_, _>>()
            .map(Self)
    }

    pub fn join(&self, name: Name) -> Self {
        let mut v = self.0.clone();
        v.push(name);
        Self(v)
    }

    pub fn components(&self) -> &[Name] {
        &self.0
    }

    /// "a/b/c", or "" for the root.
    pub fn to_slash_string(&self) -> String {
        let parts: Vec<&str> = self.0.iter().map(Name::as_str).collect();
        parts.join("/")
    }
}

/// The remote base directory. Absolute ("/data/tv") or relative to the login directory ("tv").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRoot(String);

impl RemoteRoot {
    /// Normalizes duplicate slashes, `.` components and a trailing slash. Rejects `..`, NUL and
    /// control characters: a root that climbs upward should be typed as the absolute path it means.
    pub fn parse(s: &str) -> Result<Self, PathError> {
        if s.is_empty() {
            return Err(PathError::Empty);
        }
        if s.contains('\0') {
            return Err(PathError::ContainsNul);
        }
        if s.chars().any(char::is_control) {
            return Err(PathError::ContainsControl);
        }
        let parts: Vec<&str> = s
            .split('/')
            .filter(|c| !c.is_empty() && *c != ".")
            .collect();
        if parts.contains(&"..") {
            return Err(PathError::ParentComponent);
        }
        let joined = parts.join("/");
        Ok(Self(match (s.starts_with('/'), joined.is_empty()) {
            (true, _) => format!("/{joined}"),
            (false, true) => ".".to_owned(),
            (false, false) => joined,
        }))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The remote path for `rel` under this root.
    pub fn join(&self, rel: &RelPath) -> String {
        let rel = rel.to_slash_string();
        match (self.0.as_str(), rel.is_empty()) {
            (root, true) => root.to_owned(),
            ("/", false) => format!("/{rel}"),
            (".", false) => rel,
            (root, false) => format!("{root}/{rel}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_accepts_ordinary_and_unicode_names() {
        for ok in [
            "Show.S01E01.mkv",
            "with space",
            "日本語.txt",
            "émoji 🎬",
            ".hidden",
            "...",
            "a.b",
        ] {
            assert_eq!(
                Name::new(ok).map(|n| n.as_str().to_owned()),
                Ok(ok.to_owned()),
                "{ok:?}"
            );
        }
    }

    #[test]
    fn name_rejects_traversal_and_separators() {
        assert_eq!(Name::new(""), Err(PathError::Empty));
        assert_eq!(Name::new("."), Err(PathError::DotOrDotDot));
        assert_eq!(Name::new(".."), Err(PathError::DotOrDotDot));
        assert_eq!(Name::new("a/b"), Err(PathError::ContainsSlash));
        assert_eq!(Name::new("/etc"), Err(PathError::ContainsSlash));
        assert_eq!(Name::new("../x"), Err(PathError::ContainsSlash));
    }

    #[test]
    fn name_rejects_nul_and_control_characters() {
        assert_eq!(Name::new("a\0b"), Err(PathError::ContainsNul));
        assert_eq!(Name::new("line\nbreak"), Err(PathError::ContainsControl));
        assert_eq!(Name::new("tab\there"), Err(PathError::ContainsControl));
        assert_eq!(Name::new("esc\x1b[31m"), Err(PathError::ContainsControl));
        assert_eq!(Name::new("del\x7f"), Err(PathError::ContainsControl));
        assert_eq!(Name::new("c1\u{85}"), Err(PathError::ContainsControl));
    }

    #[test]
    fn name_length_limit_is_in_bytes() {
        assert!(Name::new(&"a".repeat(255)).is_ok());
        assert_eq!(Name::new(&"a".repeat(256)), Err(PathError::TooLong));
        // 3 bytes per char: 85 chars = 255 bytes is fine, 86 = 258 bytes is not.
        assert!(Name::new(&"日".repeat(85)).is_ok());
        assert_eq!(Name::new(&"日".repeat(86)), Err(PathError::TooLong));
    }

    #[test]
    fn relpath_parses_strictly() {
        let p = RelPath::parse("Season 1/E01.mkv").expect("valid");
        assert_eq!(p.components().len(), 2);
        assert_eq!(p.to_slash_string(), "Season 1/E01.mkv");
        assert_eq!(RelPath::parse(""), Ok(RelPath::default()));
        assert_eq!(RelPath::parse("a//b"), Err(PathError::Empty));
        assert_eq!(RelPath::parse("/a"), Err(PathError::Empty));
        assert_eq!(RelPath::parse("a/"), Err(PathError::Empty));
        assert_eq!(RelPath::parse("a/./b"), Err(PathError::DotOrDotDot));
        assert_eq!(RelPath::parse("a/../../etc"), Err(PathError::DotOrDotDot));
        assert_eq!(RelPath::parse("a/b\nc"), Err(PathError::ContainsControl));
    }

    #[test]
    fn relpath_join_extends() {
        let p = RelPath::parse("a")
            .expect("valid")
            .join(Name::new("b").expect("valid"));
        assert_eq!(p.to_slash_string(), "a/b");
    }

    #[test]
    fn remote_root_normalizes() {
        let cases = [
            ("/data/tv", "/data/tv"),
            ("/data//tv/", "/data/tv"),
            ("/data/./tv", "/data/tv"),
            ("/", "/"),
            ("//", "/"),
            ("tv", "tv"),
            ("./tv/", "tv"),
            (".", "."),
        ];
        for (input, want) in cases {
            assert_eq!(
                RemoteRoot::parse(input).map(|r| r.as_str().to_owned()),
                Ok(want.to_owned()),
                "{input:?}"
            );
        }
    }

    #[test]
    fn remote_root_rejects_parent_nul_control_and_empty() {
        assert_eq!(RemoteRoot::parse(""), Err(PathError::Empty));
        assert_eq!(
            RemoteRoot::parse("/data/../etc"),
            Err(PathError::ParentComponent)
        );
        assert_eq!(RemoteRoot::parse(".."), Err(PathError::ParentComponent));
        assert_eq!(RemoteRoot::parse("/data\0"), Err(PathError::ContainsNul));
        assert_eq!(
            RemoteRoot::parse("/da\nta"),
            Err(PathError::ContainsControl)
        );
    }

    #[test]
    fn remote_root_join_stays_under_root() {
        let rel = RelPath::parse("Season 1/E01.mkv").expect("valid");
        let abs = RemoteRoot::parse("/data/tv").expect("valid");
        assert_eq!(abs.join(&rel), "/data/tv/Season 1/E01.mkv");
        assert_eq!(abs.join(&RelPath::default()), "/data/tv");
        assert_eq!(
            RemoteRoot::parse("/").expect("valid").join(&rel),
            "/Season 1/E01.mkv"
        );
        assert_eq!(
            RemoteRoot::parse(".").expect("valid").join(&rel),
            "Season 1/E01.mkv"
        );
        assert_eq!(
            RemoteRoot::parse("tv").expect("valid").join(&rel),
            "tv/Season 1/E01.mkv"
        );
    }
}
