//! What to do with a destination file that already exists (plan.md §6.5, Phase 1 step 12).
//!
//! Pure decisions, so every combination is unit tested. Times are whole seconds: SFTP v3 carries no
//! more, so comparing finer would call every pushed file changed.

use std::collections::HashSet;

/// Policy for existing destination files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Existing {
    /// Leave it alone.
    #[default]
    Skip,
    /// Replace it if size or mtime differ, unless it is newer than the source (it may hold edits).
    /// Re-running a copy after a crash makes it whole: short or missing files differ in size.
    Update,
    /// Always replace it.
    Overwrite,
    /// Keep it and write the new file under a free name: `name (1).ext`.
    Rename,
}

/// A destination as found before the transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    pub size: u64,
    pub mtime: Option<u64>,
    pub is_file: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing there: create it.
    Create,
    /// Transfer and replace what is there.
    Replace,
    /// Same size and mtime: nothing to do.
    Unchanged,
    /// Leave it (policy `Skip`).
    KeepExisting,
    /// Leave it: it is newer than the source (policy `Update`).
    KeepNewer,
    /// Transfer under a free name next to it.
    WriteBeside,
    /// Something that is not a regular file is in the way.
    NotAFile,
}

pub fn decide(
    policy: Existing,
    src_size: u64,
    src_mtime: Option<u64>,
    dst: Option<Found>,
) -> Decision {
    let Some(dst) = dst else {
        return Decision::Create;
    };
    if policy == Existing::Rename {
        return Decision::WriteBeside;
    }
    if !dst.is_file {
        return Decision::NotAFile;
    }
    match policy {
        Existing::Skip => Decision::KeepExisting,
        Existing::Overwrite => Decision::Replace,
        Existing::Rename => Decision::WriteBeside,
        Existing::Update => match (src_mtime, dst.mtime) {
            (Some(s), Some(d)) if dst.size == src_size && s == d => Decision::Unchanged,
            (Some(s), Some(d)) if d > s => Decision::KeepNewer,
            _ => Decision::Replace,
        },
    }
}

/// The first `stem (n).ext` not in `taken` (n from 1), keeping a short extension and staying within
/// 255 bytes. None if 10,000 candidates are all taken.
pub fn free_name(name: &str, taken: &HashSet<String>) -> Option<String> {
    let (stem, ext) = match name.rfind('.') {
        // A leading dot is a hidden file, not an extension; very long "extensions" are not either.
        Some(i) if i > 0 && name.len() - i <= 16 => name.split_at(i),
        _ => (name, ""),
    };
    (1..=10_000).find_map(|n| {
        let tag = format!(" ({n}){ext}");
        let mut keep = stem.len().min(255usize.saturating_sub(tag.len()));
        while !stem.is_char_boundary(keep) {
            keep -= 1;
        }
        let candidate = format!("{}{tag}", &stem[..keep]);
        (!taken.contains(&candidate)).then_some(candidate)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use Decision::*;
    use Existing::*;

    fn found(size: u64, mtime: u64) -> Option<Found> {
        Some(Found {
            size,
            mtime: Some(mtime),
            is_file: true,
        })
    }

    #[test]
    fn every_policy_against_same_older_newer_and_partial() {
        // Source: 100 bytes, mtime 1000.
        let same = found(100, 1000);
        let older = found(90, 900);
        let newer = found(100, 1100);
        let partial = found(40, 1000);
        let cases = [
            (
                Skip,
                [KeepExisting, KeepExisting, KeepExisting, KeepExisting],
            ),
            (Update, [Unchanged, Replace, KeepNewer, Replace]),
            (Overwrite, [Replace, Replace, Replace, Replace]),
            (Rename, [WriteBeside; 4]),
        ];
        for (policy, want) in cases {
            let got = [same, older, newer, partial].map(|d| decide(policy, 100, Some(1000), d));
            assert_eq!(got, want, "{policy:?}");
        }
    }

    #[test]
    fn missing_destination_is_always_created() {
        for p in [Skip, Update, Overwrite, Rename] {
            assert_eq!(decide(p, 1, Some(1), None), Create);
        }
    }

    #[test]
    fn unknown_mtimes_never_count_as_unchanged() {
        let no_mtime = Some(Found {
            size: 100,
            mtime: None,
            is_file: true,
        });
        assert_eq!(decide(Update, 100, Some(1000), no_mtime), Replace);
        assert_eq!(decide(Update, 100, None, found(100, 1000)), Replace);
    }

    #[test]
    fn directory_in_the_way() {
        let dir = Some(Found {
            size: 0,
            mtime: Some(1),
            is_file: false,
        });
        assert_eq!(decide(Overwrite, 1, Some(1), dir), NotAFile);
        assert_eq!(decide(Update, 1, Some(1), dir), NotAFile);
        assert_eq!(decide(Rename, 1, Some(1), dir), WriteBeside);
    }

    #[test]
    fn free_names() {
        let taken: HashSet<String> = ["a.txt", "a (1).txt", ".hidden"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(free_name("a.txt", &taken).as_deref(), Some("a (2).txt"));
        assert_eq!(free_name(".hidden", &taken).as_deref(), Some(".hidden (1)"));
        assert_eq!(free_name("noext", &taken).as_deref(), Some("noext (1)"));
        assert_eq!(
            free_name("x.tar.gz", &taken).as_deref(),
            Some("x.tar (1).gz")
        );
        let long = "é".repeat(127) + ".mkv";
        let n = free_name(&long, &taken).expect("name");
        assert!(n.len() <= 255 && n.ends_with(" (1).mkv"), "{n}");
    }
}
