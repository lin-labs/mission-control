/// Integration test for the trajectory file watcher.
///
/// Verifies that writing a `trajectory.md` inside a UUID subdirectory of the
/// watched data-subroot causes a `TrajectoryUpdate` event to arrive on the
/// channel within a generous 3-second window.
///
/// Marked `#[ignore]` to keep CI fast — run manually with:
///     cargo test trajectory_watcher -- --ignored --test-threads=1
///
/// macOS FSEvents can take ~1-2 s before the first event fires; the 3 s
/// timeout is chosen to stay well clear of that latency.
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
struct TrajectoryUpdate {
    uuid: String,
}

/// Mirrors `start_trajectory_watcher` from main.rs for testing purposes.
fn start_watcher(
    dir: PathBuf,
) -> anyhow::Result<(RecommendedWatcher, mpsc::UnboundedReceiver<TrajectoryUpdate>)> {
    std::fs::create_dir_all(&dir)?;

    let (tx, rx) = mpsc::unbounded_channel::<TrajectoryUpdate>();

    let debounce: Arc<Mutex<HashMap<String, Instant>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let mut watcher = notify::recommended_watcher(
        move |res: std::result::Result<notify::Event, notify::Error>| {
            let Ok(event) = res else { return };
            match event.kind {
                EventKind::Modify(_) | EventKind::Create(_) => {}
                _ => return,
            }
            for path in &event.paths {
                let file_name = path.file_name().and_then(|n| n.to_str());
                if file_name != Some("trajectory.md") {
                    continue;
                }
                let uuid = path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .map(|s| s.to_string());
                let Some(uuid) = uuid else { continue };

                let now = Instant::now();
                let mut table = debounce.lock().unwrap();
                if let Some(&last) = table.get(&uuid) {
                    if now.duration_since(last).as_millis() < 100 {
                        continue;
                    }
                }
                table.insert(uuid.clone(), now);
                drop(table);

                let _ = tx.send(TrajectoryUpdate { uuid });
            }
        },
    )?;

    watcher.watch(&dir, RecursiveMode::Recursive)?;
    Ok((watcher, rx))
}

#[tokio::test]
#[ignore]
async fn watcher_fires_on_trajectory_write() {
    let tmp = tempfile::TempDir::new().expect("create tmp dir");
    let data_root = tmp.path().to_path_buf();

    let test_uuid = "test-uuid-1234";
    let ws_dir = data_root.join(test_uuid);
    std::fs::create_dir_all(&ws_dir).expect("create workspace dir");

    let (_watcher, mut rx) = start_watcher(data_root.clone())
        .expect("start watcher");

    // Give the watcher a moment to register before writing.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Write the trajectory file — this is the external write we want to detect.
    let traj_path = ws_dir.join("trajectory.md");
    std::fs::write(&traj_path, "---\n---\n\n## Goal\n- test\n")
        .expect("write trajectory.md");

    // Wait up to 3 s for the event; macOS FSEvents can take 1-2 s.
    let deadline = Duration::from_secs(3);
    let received = tokio::time::timeout(deadline, rx.recv())
        .await
        .expect("timed out waiting for TrajectoryUpdate");

    let update = received.expect("channel closed unexpectedly");
    assert_eq!(
        update.uuid, test_uuid,
        "expected uuid {test_uuid}, got {}",
        update.uuid
    );
}

#[tokio::test]
#[ignore]
async fn watcher_ignores_non_trajectory_files() {
    let tmp = tempfile::TempDir::new().expect("create tmp dir");
    let data_root = tmp.path().to_path_buf();

    let test_uuid = "test-uuid-5678";
    let ws_dir = data_root.join(test_uuid);
    std::fs::create_dir_all(&ws_dir).expect("create workspace dir");

    let (_watcher, mut rx) = start_watcher(data_root.clone())
        .expect("start watcher");

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Write a non-trajectory file — should NOT produce an update.
    let other_path = ws_dir.join("notes.md");
    std::fs::write(&other_path, "hello").expect("write notes.md");

    // Wait briefly — no event should arrive.
    let result =
        tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
    assert!(
        result.is_err(),
        "got unexpected TrajectoryUpdate for non-trajectory file"
    );
}

#[tokio::test]
#[ignore]
async fn watcher_debounces_rapid_writes() {
    let tmp = tempfile::TempDir::new().expect("create tmp dir");
    let data_root = tmp.path().to_path_buf();

    let test_uuid = "test-uuid-9999";
    let ws_dir = data_root.join(test_uuid);
    std::fs::create_dir_all(&ws_dir).expect("create workspace dir");

    let (_watcher, mut rx) = start_watcher(data_root.clone())
        .expect("start watcher");

    tokio::time::sleep(Duration::from_millis(200)).await;

    let traj_path = ws_dir.join("trajectory.md");
    // Write the file 5 times in quick succession (< 100 ms apart).
    for i in 0..5u8 {
        std::fs::write(&traj_path, format!("write {i}")).expect("write");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Collect events for 1 s — debouncing should collapse these into 1 or 2 events.
    let mut count = 0usize;
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(_)) => count += 1,
            _ => break,
        }
    }
    // We expect at most 2 events for 5 rapid writes (debounce should absorb most).
    assert!(
        count <= 2,
        "expected ≤ 2 debounced events, got {count}"
    );
    assert!(count >= 1, "expected at least 1 event");
}
