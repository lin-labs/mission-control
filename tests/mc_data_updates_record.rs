use chrono::{DateTime, Duration, TimeZone, Utc};
use mission_control::mc_data::updates::artifacts::{Artifact, ArtifactKind};
use mission_control::mc_data::updates::record::*;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0).unwrap()
}

fn segment(n: usize) -> Segment {
    Segment {
        segment: format!("s{n}"),
        intent: "ship the thing".into(),
        latest: "tests pass".into(),
        latest_at: Some(t0()),
        state: SegState::WaitingOnMe,
        needs_me: None,
        user_inputs: vec![UserInput {
            at: t0(),
            text: "hello".into(),
        }],
        agent_turns: vec![AgentTurn {
            at: t0(),
            injected: true,
        }],
        artifacts: vec![Artifact {
            kind: ArtifactKind::Pr,
            reference: "o/r#1".into(),
            at: t0(),
        }],
        last_refine_at: None,
    }
}

fn file(segments: Vec<Segment>) -> SessionFile {
    SessionFile {
        host: "devbox".into(),
        session: "abc".into(),
        agent: "claude".into(),
        transcript: "/tmp/abc.jsonl".into(),
        workspace: None,
        surface: Some("agents:1".into()),
        status: None,
        segments,
        updated_at: t0(),
    }
}

#[test]
fn roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let f = file(vec![segment(1)]);
    save(dir.path(), &f).unwrap();
    assert_eq!(load(dir.path(), "abc"), Some(f));
    assert_eq!(load(dir.path(), "missing"), None);
    let raw = std::fs::read_to_string(dir.path().join("abc.json")).unwrap();
    assert!(raw.contains("\"waiting_on_me\"") && raw.contains("\"loop\":true"));
}

#[test]
fn save_is_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let f = file(vec![segment(1)]);
    for _ in 0..3 {
        save(dir.path(), &f).unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["abc.json"]);
        let raw = std::fs::read(dir.path().join("abc.json")).unwrap();
        serde_json::from_slice::<serde_json::Value>(&raw).unwrap();
    }
}

#[test]
fn large_session_trimmed_to_cap() {
    let dir = tempfile::tempdir().unwrap();
    let mut seg = segment(1);
    seg.user_inputs = (0..500)
        .map(|i| UserInput {
            at: t0() + Duration::seconds(i),
            text: format!("{i:04}{}", "é".repeat(996)),
        })
        .collect();
    save(dir.path(), &file(vec![seg])).unwrap();
    let len = std::fs::metadata(dir.path().join("abc.json"))
        .unwrap()
        .len();
    assert!(len as usize <= MAX_FILE_BYTES, "{len}");
    let got = load(dir.path(), "abc").unwrap();
    let inputs = &got.segments[0].user_inputs;
    assert!(inputs.last().unwrap().text.starts_with("0499"));
    assert!(inputs.len() < 500);
    assert!(inputs.iter().all(|u| u.text.chars().count() <= 300));
}

#[test]
fn trim_keeps_intent_and_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = segment(1);
    let mut b = segment(2);
    for s in [&mut a, &mut b] {
        s.user_inputs = (0..300)
            .map(|i| UserInput {
                at: t0() + Duration::seconds(i),
                text: "x".repeat(1000),
            })
            .collect();
        s.agent_turns = (0..500)
            .map(|i| AgentTurn {
                at: t0() + Duration::seconds(i),
                injected: false,
            })
            .collect();
    }
    let f = file(vec![a, b]);
    save(dir.path(), &f).unwrap();
    let got = load(dir.path(), "abc").unwrap();
    for (g, o) in got.segments.iter().zip(&f.segments) {
        assert_eq!(
            (&g.intent, &g.latest, &g.artifacts),
            (&o.intent, &o.latest, &o.artifacts)
        );
        assert!(!g.user_inputs.is_empty());
        assert!(g.agent_turns.len() <= 200);
    }
    assert_eq!(
        got.segments[0].agent_turns.last(),
        f.segments[0].agent_turns.last()
    );
}

#[test]
fn many_segments_drop_done_then_oldest() {
    let dir = tempfile::tempdir().unwrap();
    let segs: Vec<_> = (0..200usize)
        .map(|n| {
            let mut s = segment(n + 1);
            // Newest segment is the last; every 3rd is Done.
            s.latest_at = Some(t0() + Duration::seconds(n as i64));
            s.state = if n % 3 == 0 && n != 199 {
                SegState::Done
            } else {
                SegState::Working
            };
            s.user_inputs[0].text = "y".repeat(300);
            s.artifacts = (0..4)
                .map(|k| Artifact {
                    kind: ArtifactKind::Pr,
                    reference: format!("o/r#{n}{k}"),
                    at: t0(),
                })
                .collect();
            s
        })
        .collect();
    save(dir.path(), &file(segs)).unwrap();
    let len = std::fs::metadata(dir.path().join("abc.json"))
        .unwrap()
        .len();
    assert!(len as usize <= MAX_FILE_BYTES, "{len}");
    let got = load(dir.path(), "abc").unwrap().segments;
    assert!(got.len() < 200);
    assert_eq!(got.last().unwrap().segment, "s200");
    // 67 Done + 133 Working originally: a Working drop implies no Done is left.
    let done_left = got.iter().filter(|s| s.state == SegState::Done).count();
    let working_left = got.iter().filter(|s| s.state == SegState::Working).count();
    assert!(done_left < 67, "no Done segment was dropped");
    assert!(
        working_left == 133 || done_left == 0,
        "{done_left} {working_left}"
    );
}

#[test]
fn invalid_session_id_rejected() {
    let dir = tempfile::tempdir().unwrap();
    for bad in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
        let mut f = file(vec![segment(1)]);
        f.session = bad.into();
        let e = save(dir.path(), &f).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{bad:?}");
        assert!(load(dir.path(), bad).is_none());
    }
}

#[test]
fn oversize_without_segments_still_saves() {
    let dir = tempfile::tempdir().unwrap();
    let mut f = file(vec![]);
    f.transcript = "/t/".repeat(17_000).into();
    save(dir.path(), &f).unwrap();
    assert_eq!(load(dir.path(), "abc"), Some(f));
}
