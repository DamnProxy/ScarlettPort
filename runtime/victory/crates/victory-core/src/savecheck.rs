//! Offline check of a provider's game-save folder (the layout written by
//! `victory-hle`'s XGameSave module): each container's newest version must
//! have a manifest, every blob it names, and, for the title's `ASAV` saves,
//! a header blob that agrees with its data blob (size, timestamp, packed
//! sizes). Leftovers a kill or a scanner left (`v<N>.new`, empty `v<N>`)
//! are reported. Nothing is changed except by [`set_aside`], which renames
//! a container away (undo: rename it back).

use std::path::{Path, PathBuf};

/// One problem found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub path: PathBuf,
    pub what: String,
    /// The container cannot be read as saved (not just untidy).
    pub fatal: bool,
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn parse_manifest(bytes: &[u8]) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let len = u32_at(rest, 0)? as usize;
        out.push(String::from_utf8(rest.get(4..4 + len)?.to_vec()).ok()?);
        rest = &rest[4 + len..];
    }
    (out.len() >= 2).then_some(out)
}

/// Checks an `ASAV` data blob against its `Save Game Header` blob.
/// Layout (observed): data `ASAV`, u32 2, .., u32 packed size at 0x18
/// (bytes after 0x28, 0 when unpacked), u32 unpacked size at 0x1c, u64
/// unix time at 0x20, u32 size at 0x168; header u32 6, u32 data size,
/// u64 checksum, u64 the same time, then 0x18 and 0x1c of the data.
pub fn check_asav(data: &[u8], header: &[u8]) -> Vec<String> {
    let mut bad = Vec::new();
    if data.get(..4) != Some(b"ASAV") {
        return bad; // not this title's format: nothing to say
    }
    let len = data.len() as u64;
    if header.len() < 0x24 {
        bad.push(format!("header blob is {} bytes", header.len()));
        return bad;
    }
    if u32_at(header, 4).map(u64::from) != Some(len) {
        bad.push(format!("header says {:?} bytes, data has {len}", u32_at(header, 4)));
    }
    if u64_at(header, 0x10) != u64_at(data, 0x20) {
        bad.push("header and data times differ (from different saves)".into());
    }
    if header.get(0x1c..0x24) != data.get(0x18..0x20) {
        bad.push("header and data packed sizes differ".into());
    }
    if let Some(size) = u32_at(data, 0x168).filter(|&s| s != 0 && u64::from(s) != len) {
        bad.push(format!("data says {size} bytes at 0x168, has {len}"));
    }
    if let Some(packed) = u32_at(data, 0x18).filter(|&p| p != 0) {
        if u64::from(packed) + 0x28 != len {
            bad.push(format!("packed size {packed} + 0x28 != {len} (cut short?)"));
        }
    }
    bad
}

/// Checks one container folder (`<name>~<hash>`).
pub fn check_container(dir: &Path) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut note = |path: PathBuf, what: String, fatal| out.push(Finding { path, what, fatal });
    let mut versions = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        if file.ends_with(".new") || file.ends_with(".bad") {
            note(entry.path(), "leftover version folder".into(), false);
        } else if let Some(n) = file.strip_prefix('v').and_then(|n| n.parse::<u64>().ok()) {
            versions.push((n, entry.path()));
        }
    }
    versions.sort();
    let Some((_, newest)) = versions.pop() else {
        note(dir.to_path_buf(), "no committed version".into(), true);
        return out;
    };
    for (_, old) in versions {
        let empty = std::fs::read_dir(&old).map_or(true, |mut d| d.next().is_none());
        let what = if empty { "empty older version folder" } else { "older version folder" };
        note(old, what.into(), false);
    }
    let Some(names) = std::fs::read(newest.join("manifest")).ok().and_then(|b| parse_manifest(&b)) else {
        note(newest, "manifest missing or damaged".into(), true);
        return out;
    };
    let blobs = &names[2..];
    let mut read = Vec::new();
    for (i, name) in blobs.iter().enumerate() {
        match std::fs::read(newest.join(i.to_string())) {
            Ok(b) if b.is_empty() => note(newest.join(i.to_string()), format!("blob {name:?} is empty"), true),
            Ok(b) => read.push((name.as_str(), b)),
            Err(e) => note(newest.join(i.to_string()), format!("blob {name:?}: {e}"), true),
        }
    }
    let find = |n: &str| read.iter().find(|(b, _)| *b == n).map(|(_, d)| d);
    if let (Some(data), Some(header)) = (find("Save Game Data"), find("Save Game Header")) {
        for what in check_asav(data, header) {
            note(newest.clone(), what, true);
        }
    }
    out
}

/// Checks every container under a provider folder.
pub fn check_provider(root: &Path) -> Vec<Finding> {
    let mut dirs: Vec<_> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.file_name().is_some_and(|f| f.to_string_lossy().contains('~')))
        .filter(|p| !p.to_string_lossy().ends_with(".gone"))
        .collect();
    dirs.sort();
    dirs.iter().flat_map(|d| check_container(d)).collect()
}

/// Moves a container folder out of the title's sight, into
/// `<provider>/.aside/<folder>-<unix secs>` (the save module ignores names
/// with a dot, as it does `.backups`). Returns the new path. Undo: move it
/// back.
pub fn set_aside(container: &Path, unix_secs: u64) -> std::io::Result<PathBuf> {
    let name = container.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let aside = container.with_file_name(".aside");
    std::fs::create_dir_all(&aside)?;
    let to = aside.join(format!("{name}-{unix_secs}"));
    std::fs::rename(container, &to)?;
    Ok(to)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(names: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for s in names {
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        out
    }

    fn asav(len: usize, packed: bool) -> (Vec<u8>, Vec<u8>) {
        let mut d = vec![0u8; len];
        d[..4].copy_from_slice(b"ASAV");
        if packed {
            d[0x18..0x1c].copy_from_slice(&((len - 0x28) as u32).to_le_bytes());
            d[0x1c..0x20].copy_from_slice(&900_000u32.to_le_bytes());
        }
        d[0x20..0x28].copy_from_slice(&1_795_546_298u64.to_le_bytes());
        d[0x168..0x16c].copy_from_slice(&(len as u32).to_le_bytes());
        let mut h = vec![0u8; 296];
        h[..4].copy_from_slice(&6u32.to_le_bytes());
        h[4..8].copy_from_slice(&(len as u32).to_le_bytes());
        h[0x10..0x18].copy_from_slice(&d[0x20..0x28]);
        h[0x1c..0x24].copy_from_slice(&d[0x18..0x20]);
        (d, h)
    }

    fn container(root: &Path, data: &[u8], header: &[u8]) -> PathBuf {
        let dir = root.join("SARC0015~f3ad35e1a05a7da0");
        let v = dir.join("v1");
        std::fs::create_dir_all(&v).unwrap();
        std::fs::write(v.join("manifest"), manifest(&["SARC0015", "SARC0015", "Save Game Data", "Save Game Header"])).unwrap();
        std::fs::write(v.join("0"), data).unwrap();
        std::fs::write(v.join("1"), header).unwrap();
        dir
    }

    #[test]
    fn a_consistent_save_passes() {
        let t = tempfile::tempdir().unwrap();
        let (d, h) = asav(0x3bad0, true);
        container(t.path(), &d, &h);
        assert_eq!(check_provider(t.path()), vec![]);
    }

    #[test]
    fn a_cut_short_save_is_fatal() {
        let t = tempfile::tempdir().unwrap();
        let (d, h) = asav(0x3bad0, true);
        container(t.path(), &d[..0x20000], &h);
        let f = check_provider(t.path());
        assert!(f.iter().any(|f| f.fatal && f.what.contains("cut short")), "{f:?}");
    }

    #[test]
    fn a_header_from_another_save_is_fatal() {
        let t = tempfile::tempdir().unwrap();
        let (d, mut h) = asav(0x7dc, false);
        h[0x10] ^= 1;
        container(t.path(), &d, &h);
        assert!(check_provider(t.path()).iter().any(|f| f.fatal && f.what.contains("times")));
    }

    #[test]
    fn leftovers_are_reported_not_fatal() {
        let t = tempfile::tempdir().unwrap();
        let (d, h) = asav(0x7dc, false);
        let dir = container(t.path(), &d, &h);
        std::fs::rename(dir.join("v1"), dir.join("v3")).unwrap();
        std::fs::create_dir_all(dir.join("v2")).unwrap();
        std::fs::create_dir_all(dir.join("v4.new")).unwrap();
        let f = check_provider(t.path());
        assert_eq!(f.len(), 2, "{f:?}");
        assert!(f.iter().all(|f| !f.fatal));
    }

    #[test]
    fn set_aside_hides_and_can_be_undone() {
        let t = tempfile::tempdir().unwrap();
        let (d, h) = asav(0x7dc, false);
        let dir = container(t.path(), &d, &h);
        let to = set_aside(&dir, 1).unwrap();
        assert!(check_provider(t.path()).is_empty() && !dir.exists());
        std::fs::rename(&to, &dir).unwrap();
        assert!(dir.exists());
    }
}
