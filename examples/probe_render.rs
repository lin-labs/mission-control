//! T3 live-verification probe.
//!
//! Builds an in-memory workspace with a Claude surface, a Shell surface that
//! recently hosted Codex (last-agent file backed), and two goals — one
//! assigned to the Claude surface, one to the Shell surface. Renders the
//! resulting trajectory.md and prints it so a human (or the controller)
//! can eyeball the glyphs and badges in real terminal output.

use chrono::Utc;
use std::path::PathBuf;

use mission_control::mc_data::goals_json::{GoalEntry, GoalsFile, normalize_text};
use mission_control::mc_data::paths;
use mission_control::mc_data::surface_kind::{
    self, LastAgent, SurfaceKind, effective_kind, write_last_agent,
};
use mission_control::mc_data::surface_render::{format_goal_badge, format_surface_text};
use mission_control::mc_data::trajectory::{
    Item, SECTION_CURRENT_SURFACES, SECTION_GOALS, SECTION_MISSION, TrajectoryDoc,
};

fn main() {
    let uuid = "probe-render-t3";

    // Clean any leftover state so the probe is reproducible.
    let dir = paths::workspace_dir(uuid);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create probe workspace dir");

    // Surface 1: live Claude.
    write_last_agent(uuid, "surface:11", SurfaceKind::Claude).expect("seed last-agent");
    // Surface 2: live Shell, but a fresh Codex last-agent so the glyph
    // stays an agent glyph (this is the "just-exited" / dim scenario).
    write_last_agent(uuid, "surface:22", SurfaceKind::Codex).expect("seed last-agent codex");

    let s11_kind_current = SurfaceKind::Claude;
    let s22_kind_current = SurfaceKind::Shell;
    let s11_eff = effective_kind(uuid, "surface:11", s11_kind_current);
    let s22_eff = effective_kind(uuid, "surface:22", s22_kind_current);

    // Build a goals.json with one assignment per surface.
    let mut goals = GoalsFile::default();
    goals.goals.push(GoalEntry {
        id: None,
        text: "Wire up T3 rendering".into(),
        text_norm: normalize_text("Wire up T3 rendering"),
        assigned_surface_ref: "surface:11".into(),
        assigned_agent_kind: SurfaceKind::Claude,
        dispatched_at: Utc::now(),
        completed_at: None,
    });
    goals.goals.push(GoalEntry {
        id: None,
        text: "Investigate macOS hotkey regression".into(),
        text_norm: normalize_text("Investigate macOS hotkey regression"),
        assigned_surface_ref: "surface:22".into(),
        assigned_agent_kind: SurfaceKind::Codex,
        dispatched_at: Utc::now(),
        completed_at: None,
    });
    goals.save(uuid).expect("save goals.json");

    // Build the trajectory doc.
    let mut doc = TrajectoryDoc::skeleton(uuid, "probe", "probe");
    doc.replace_section_items(
        SECTION_MISSION,
        vec![Item {
            text: "Ship surface peek".into(),
            is_checkbox: false,
            checked: None,
            surface_id: None,
        }],
    );

    let surface_items = vec![
        Item {
            text: format_surface_text(s11_eff, "claude · mbp · working", &goals, "surface:11"),
            is_checkbox: false,
            checked: None,
            surface_id: Some("surface:11".into()),
        },
        Item {
            text: format_surface_text(s22_eff, "shell · mbp · idle", &goals, "surface:22"),
            is_checkbox: false,
            checked: None,
            surface_id: Some("surface:22".into()),
        },
    ];
    doc.replace_section_items(SECTION_CURRENT_SURFACES, surface_items);

    // Goals & Progress: append badges using format_goal_badge.
    let goal_rows = [
        ("Wire up T3 rendering", false),
        ("Investigate macOS hotkey regression", false),
        ("Land T0 rename", true),
    ];
    let goal_items: Vec<Item> = goal_rows
        .into_iter()
        .map(|(text, done)| {
            let mut body = text.to_string();
            if let Some(badge) = format_goal_badge(&goals, text) {
                body.push_str(&badge);
            }
            Item {
                text: body,
                is_checkbox: true,
                checked: Some(done),
                surface_id: None,
            }
        })
        .collect();
    doc.replace_section_items(SECTION_GOALS, goal_items);

    let traj_path = paths::trajectory_path(uuid);
    doc.save_to_file(&traj_path).expect("save trajectory.md");

    println!("── trajectory.md (uuid={uuid}) ──────────────────────────────");
    let body = std::fs::read_to_string(&traj_path).expect("read trajectory.md");
    println!("{}", body);

    // Eyeball helpers: print the per-surface effective kinds.
    println!("── effective kinds ───────────────────────────────────────────");
    println!(
        "surface:11 current={:?} effective={:?} (live agent)",
        s11_kind_current, s11_eff
    );
    println!(
        "surface:22 current={:?} effective={:?} (just-exited Codex; dim)",
        s22_kind_current, s22_eff
    );

    // Demonstrate that a workspace with NO goals.json renders unchanged
    // (no `← goal:` or `→` appears anywhere).
    let bare_uuid = "probe-render-t3-bare";
    let _ = std::fs::remove_dir_all(paths::workspace_dir(bare_uuid));
    std::fs::create_dir_all(paths::workspace_dir(bare_uuid)).unwrap();
    let bare_goals = GoalsFile::load(bare_uuid); // empty default

    let bare = format_surface_text(
        SurfaceKind::Claude,
        "claude · mbp · working",
        &bare_goals,
        "surface:99",
    );
    println!("── no-goals-no-change check ─────────────────────────────────");
    println!("{}", bare);
    assert!(!bare.contains("← goal:"));
    assert!(format_goal_badge(&bare_goals, "Anything").is_none());

    // Sanity: silence unused-import warnings even when probe runs in CI.
    let _: &LastAgent = &LastAgent {
        kind: SurfaceKind::Claude,
        ts: Utc::now(),
    };
    let _ = surface_kind::read_last_agent(uuid, "surface:11");
    let _ = PathBuf::from("");
}
