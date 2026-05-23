use crate::mc_data::paths;
use crate::mc_data::trajectory::TrajectoryDoc;
use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;

/// Write the current TrajectoryDoc to histories/trajectory-N.md.
/// N is the snapshot number — caller-supplied (typically frontmatter.snapshot
/// or 1 + max(existing snapshots)).
pub fn write_snapshot(uuid: &str, n: u32, doc: &TrajectoryDoc) -> Result<PathBuf> {
    let dir = paths::histories_dir(uuid);
    fs::create_dir_all(&dir).with_context(|| format!("create {dir:?}"))?;
    let path = dir.join(format!("trajectory-{n}.md"));
    let tmp = dir.join(format!("trajectory-{n}.md.tmp"));
    fs::write(&tmp, doc.to_markdown()).with_context(|| format!("write {tmp:?}"))?;
    fs::rename(&tmp, &path).with_context(|| format!("rename {tmp:?} -> {path:?}"))?;
    Ok(path)
}

/// Find the highest existing snapshot N in histories/. Returns 0 if none.
pub fn highest_snapshot(uuid: &str) -> Result<u32> {
    let dir = paths::histories_dir(uuid);
    let read = match fs::read_dir(&dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(anyhow::Error::from(e).context(format!("read dir {dir:?}"))),
    };
    let mut max_n: u32 = 0;
    for entry in read {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(rest) = name.strip_prefix("trajectory-") {
            if let Some(num) = rest.strip_suffix(".md") {
                if let Ok(n) = num.parse::<u32>() {
                    if n > max_n {
                        max_n = n;
                    }
                }
            }
        }
    }
    Ok(max_n)
}
