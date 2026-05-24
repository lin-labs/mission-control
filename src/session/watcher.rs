use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use tokio::sync::mpsc;

pub struct SessionWatcher {
    _watcher: RecommendedWatcher,
}

#[derive(Debug, Clone)]
pub struct FileChanged {
    pub path: PathBuf,
}

impl SessionWatcher {
    pub fn new(dir: PathBuf, tx: mpsc::UnboundedSender<FileChanged>) -> Result<Self> {
        let mut watcher =
            notify::recommended_watcher(move |res: std::result::Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    match event.kind {
                        EventKind::Modify(_) | EventKind::Create(_) => {
                            for path in event.paths {
                                if path.extension().is_some_and(|e| e == "md") {
                                    let _ = tx.send(FileChanged { path });
                                }
                            }
                        }
                        _ => {}
                    }
                }
            })?;

        watcher.watch(&dir, RecursiveMode::NonRecursive)?;

        Ok(SessionWatcher { _watcher: watcher })
    }
}
