use mission_control::mc_data::trajectory::TrajectoryDoc;
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use ratatui::layout::Rect;

const SAMPLE: &str = "---
workspace: predinvest
---

## Goal
- Build self-improvement-enabled investment agent

## Current surfaces
- claude · mbp · working · writing tests              <!-- mc:surface:sid-1 -->

## Tasks & Progress
- [x] sprint-01 done
- [ ] sprint-02
";

fn buf_dump(terminal: &Terminal<TestBackend>) -> String {
    let buf = terminal.backend().buffer();
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .filter_map(|x| {
                    buf.cell((x, y)).map(|c| c.symbol().chars().next().unwrap_or(' '))
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn render_emits_section_headers_and_items() {
    let doc = TrajectoryDoc::parse(SAMPLE).unwrap();
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|f| {
            mission_control::tui::trajectory_view::render(
                f,
                Rect::new(0, 0, 80, 20),
                Some(&doc),
                0,
                false,
            );
        })
        .unwrap();

    let dump = buf_dump(&terminal);
    assert!(dump.contains("Goal"), "missing Goal header: {dump}");
    assert!(dump.contains("Current surfaces"), "missing Current surfaces header");
    assert!(dump.contains("Tasks & Progress"), "missing Tasks header");
    assert!(dump.contains("Build self-improvement"), "missing Goal item");
    assert!(dump.contains("writing tests"), "missing surface text");
    assert!(dump.contains("sprint-01 done"), "missing task text");
    assert!(!dump.contains("mc:surface:"), "leaked surface comment into UI");
}

#[test]
fn render_with_no_doc_shows_placeholder() {
    let backend = TestBackend::new(80, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            mission_control::tui::trajectory_view::render(
                f,
                Rect::new(0, 0, 80, 10),
                None,
                0,
                false,
            );
        })
        .unwrap();
    let dump = buf_dump(&terminal);
    assert!(dump.contains("No trajectory") || dump.contains("no trajectory"));
}
