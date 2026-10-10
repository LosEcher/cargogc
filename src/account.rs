//! Inode-honest accounting: how much does deleting these paths actually return?
//!
//! The number that answers "how much do I get back" is `du` — `st_blocks × 512`,
//! deduplicated by `(st_dev, st_ino)`. The number that explains "why did it look so
//! big" is the logical size (`st_size`, summed per path, no dedup). Both are reported,
//! together with the bytes that stay allocated because the inode is still referenced
//! from outside the delete set.
//!
//! Why the third number exists, measured on a real tree: deleting 164 incremental
//! entries changed the *subtree* `du` by 9.82 GiB, while the enclosing `target` tree
//! and an inode-level simulation both said 8.55 GiB. The missing 1.30 GiB was
//! `metadata.rmeta`-style files hardlinked from `deps/` — the path went away, the
//! blocks did not. Reporting the subtree `du` as "freed" would have promised space
//! that never came back.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// One inode, as seen while walking a delete set.
#[derive(Debug, Clone, Copy)]
struct Inode {
    blocks: u64,
    /// Hard link count from the filesystem.
    nlink: u64,
    /// How many times this inode was reached inside the delete set.
    seen: u64,
    /// Index of the first delete-set item that reached it (for per-item attribution).
    first_item: usize,
}

/// Per-item numbers. `freed_du` is attributed (each inode is credited to the first item
/// that references it, so the per-item numbers add up to the total instead of each one
/// restating the shared bytes); `logical` is the plain per-path sum for that item.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ItemBytes {
    pub freed_du: u64,
    pub logical: u64,
    pub shared_du: u64,
}

/// Class- and run-level totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub items: usize,
    pub freed_du: u64,
    pub logical: u64,
    pub shared_du: u64,
    pub shared_inodes: usize,
}

/// account — walk `paths`, returning per-item numbers and the totals.
///
/// Paths that no longer exist count as zero bytes rather than an error: they may have
/// been removed between planning and accounting, and a maintenance tool must not fail
/// over that.
pub fn account(paths: &[PathBuf]) -> io::Result<(Vec<ItemBytes>, Totals)> {
    let mut inodes: HashMap<(u64, u64), Inode> = HashMap::new();
    let mut per_item = vec![ItemBytes::default(); paths.len()];
    let mut logical_by_item = vec![0u64; paths.len()];

    for (index, path) in paths.iter().enumerate() {
        walk(path, index, &mut inodes, &mut logical_by_item, 0)?;
    }

    for inode in inodes.values() {
        if inode.nlink <= inode.seen {
            per_item[inode.first_item].freed_du += inode.blocks;
        } else {
            per_item[inode.first_item].shared_du += inode.blocks;
        }
    }
    for (index, logical) in logical_by_item.into_iter().enumerate() {
        per_item[index].logical = logical;
    }

    let mut totals = Totals {
        items: paths.len(),
        ..Totals::default()
    };
    for item in &per_item {
        totals.freed_du += item.freed_du;
        totals.logical += item.logical;
        totals.shared_du += item.shared_du;
    }
    totals.shared_inodes = inodes.values().filter(|i| i.nlink > i.seen).count();

    Ok((per_item, totals))
}

/// du_of — the `du` of one tree, deduplicating inodes *within* that tree.
///
/// This is the same notion `du -sk` reports, and it is what the size cap compares
/// against; it deliberately does not care about links leaving the tree.
pub fn du_of(path: &Path) -> io::Result<u64> {
    let mut seen: HashMap<(u64, u64), u64> = HashMap::new();
    collect_du(path, &mut seen, 0)?;
    Ok(seen.values().sum())
}

fn collect_du(path: &Path, seen: &mut HashMap<(u64, u64), u64>, depth: usize) -> io::Result<()> {
    if depth > 64 {
        return Ok(());
    }
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    seen.entry((dev_of(&meta), ino_of(&meta)))
        .or_insert(blocks_of(&meta));
    if meta.is_dir() {
        for entry in read_entries(path)? {
            collect_du(&entry, seen, depth + 1)?;
        }
    }
    Ok(())
}

fn walk(
    path: &Path,
    item: usize,
    inodes: &mut HashMap<(u64, u64), Inode>,
    logical_by_item: &mut [u64],
    depth: usize,
) -> io::Result<()> {
    // Symlinks in the tree are counted as themselves (this is `du`, not `du -L`), so a
    // link cannot be used to escape the delete set.
    if depth > 64 {
        return Ok(());
    }
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };

    logical_by_item[item] += size_of(&meta);
    let key = (dev_of(&meta), ino_of(&meta));
    let entry = inodes.entry(key).or_insert(Inode {
        blocks: blocks_of(&meta),
        // A directory's link count counts its subdirectories, not hardlinks to it
        // (directories cannot be hardlinked), so it must never be read as "shared":
        // deleting the directory does free its blocks.
        nlink: if meta.is_dir() { 1 } else { nlink_of(&meta) },
        seen: 0,
        first_item: item,
    });
    entry.seen += 1;

    if meta.is_dir() {
        for child in read_entries(path)? {
            walk(&child, item, inodes, logical_by_item, depth + 1)?;
        }
    }
    Ok(())
}

fn read_entries(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(err),
    };
    for entry in entries {
        match entry {
            Ok(entry) => out.push(entry.path()),
            Err(_) => continue,
        }
    }
    Ok(out)
}

#[cfg(unix)]
fn dev_of(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.dev()
}

#[cfg(unix)]
fn ino_of(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.ino()
}

#[cfg(unix)]
fn nlink_of(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.nlink()
}

#[cfg(unix)]
fn blocks_of(meta: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.blocks() * 512
}

// On platforms without the Unix metadata extension (Windows), fall back to the logical
// size and a link count of one. `apply` already refuses there (see `gate`), so these
// numbers are only ever used by `plan`; the README states the boundary.
#[cfg(not(unix))]
fn dev_of(_meta: &fs::Metadata) -> u64 {
    0
}

#[cfg(not(unix))]
fn ino_of(meta: &fs::Metadata) -> u64 {
    // No inode numbers: identify by size + mtime so that two different files do not
    // collapse into one. Deduplication is best-effort on this platform.
    use std::time::UNIX_EPOCH;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    mtime ^ meta.len()
}

#[cfg(not(unix))]
fn nlink_of(_meta: &fs::Metadata) -> u64 {
    1
}

#[cfg(not(unix))]
fn blocks_of(meta: &fs::Metadata) -> u64 {
    meta.len()
}

fn size_of(meta: &fs::Metadata) -> u64 {
    meta.len()
}

/// human — bytes rendered for a human, 1024-based.
pub fn human(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value >= KIB * KIB * KIB {
        format!("{:.2} GiB", value / (KIB * KIB * KIB))
    } else if value >= KIB * KIB {
        format!("{:.1} MiB", value / (KIB * KIB))
    } else if value >= KIB {
        format!("{:.1} KiB", value / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// inflation — how much the logical byte count overstates what deleting returns.
pub fn inflation(logical: u64, freed_du: u64) -> f64 {
    if freed_du == 0 {
        return if logical == 0 { 1.0 } else { f64::INFINITY };
    }
    logical as f64 / freed_du as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cargogc-account-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_hardlink_shared_outside_the_delete_set_is_not_freed() {
        let dir = scratch("hardlink");
        let set = dir.join("set");
        fs::create_dir_all(&set).unwrap();
        fs::write(set.join("blob"), vec![7u8; 8192]).unwrap();
        let outside = dir.join("outside");
        fs::hard_link(set.join("blob"), &outside).unwrap();

        let (items, totals) = account(std::slice::from_ref(&set)).unwrap();
        assert!(
            items[0].freed_du < totals.logical,
            "a shared inode must not be reported as freed"
        );
        // Assert on the blob, not on a total that encodes one filesystem's layout: a
        // directory's *own* inode block may legitimately be freed, and its size is
        // filesystem-dependent — ubuntu CI measured 4096 B (ext4) where APFS reports 0.
        // The first version of this test asserted `freed_du == 0` and went red on Linux
        // only, which is exactly the class of assumption this suite must not contain.
        assert!(
            items[0].freed_du < 8192,
            "the shared blob (8192 B) must not be credited as freed; at most a directory's \
             own block may be (got {} bytes)",
            items[0].freed_du
        );
        assert_eq!(totals.shared_inodes, 1);
        assert!(totals.shared_du > 0);
        // The directory's own st_size is part of the logical sum, so compare against a
        // floor rather than an exact number.
        assert!(
            totals.logical >= 8192,
            "logical counts the blob: {}",
            totals.logical
        );

        fs::remove_file(&outside).unwrap();
        let (_, totals) = account(&[set]).unwrap();
        assert!(
            totals.freed_du >= 8192,
            "once the outer link is gone the blocks are ours"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn two_links_inside_the_delete_set_count_once_and_are_freed() {
        let dir = scratch("internal-hardlink");
        let set = dir.join("set");
        fs::create_dir_all(&set).unwrap();
        fs::write(set.join("a"), vec![1u8; 8192]).unwrap();
        fs::hard_link(set.join("a"), set.join("b")).unwrap();

        let (_, totals) = account(&[set]).unwrap();
        assert_eq!(totals.shared_inodes, 0, "both links are inside the set");
        assert!(
            totals.logical >= 16384,
            "logical is per path: {}",
            totals.logical
        );
        assert!((8192..16384).contains(&totals.freed_du), "du deduplicates");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn du_of_deduplicates_within_the_tree() {
        let dir = scratch("du");
        fs::write(dir.join("a"), vec![3u8; 8192]).unwrap();
        fs::hard_link(dir.join("a"), dir.join("b")).unwrap();
        let du = du_of(&dir).unwrap();
        assert!(
            (8192..16384).contains(&du),
            "du must count the inode once, got {du}"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_paths_account_as_zero_and_never_fail() {
        let dir = scratch("missing");
        let (items, totals) = account(&[dir.join("nope")]).unwrap();
        assert_eq!(items[0], ItemBytes::default());
        assert_eq!(totals.freed_du, 0);
        assert_eq!(totals.items, 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn human_and_inflation_are_stable() {
        assert_eq!(human(0), "0 B");
        assert_eq!(human(2048), "2.0 KiB");
        assert_eq!(human(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(human(3 * 1024 * 1024 * 1024), "3.00 GiB");
        assert!((inflation(122, 100) - 1.22).abs() < 1e-9);
        assert_eq!(inflation(0, 0), 1.0);
    }
}
