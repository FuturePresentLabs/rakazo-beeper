//! The roster is data, not source: a new vertical in `verticals.toml` must
//! appear without a code edit, and a paused one must stay visible but unrouted.

use std::collections::BTreeMap;
use std::fs;

use rakazo_beeper::orch::{
    Kind, ORCH_KEY,
    roster::{from_parts, load_verticals, parse_live_tsv},
    route_key,
};

fn write_verticals(extra: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("verticals.toml");
    fs::write(
        &path,
        format!(
            r#"
[[vertical]]
name = "fpl"
owner = "fpl"
roots = ["/nonexistent/fpl"]
scope = "repo:FuturePresentLabs/rakazo-beeper"
max_workers = 2
briefs = "pm/fpl/briefs/"
cadence_s = 1800

[[vertical]]
name = "marbles"
owner = "marbles"
roots = ["/nonexistent/marbles"]
scope = "repo:FuturePresentLabs/marbles"

[[vertical]]
name = "transmog"
owner = "transmog"
paused = true
{extra}
"#
        ),
    )
    .unwrap();
    (dir, path)
}

#[test]
fn a_new_vertical_needs_only_the_data_file() {
    let (_dir, path) = write_verticals(
        r#"
[[vertical]]
name = "brand-new"
owner = "brand-new"
roots = ["/nonexistent/brand-new"]
scope = "repo:FuturePresentLabs/brand-new"
max_workers = 2
briefs = "pm/brand-new/briefs/"
cadence_s = 1800
"#,
    );
    let verticals = load_verticals(&path).expect("parse verticals");
    let entries = from_parts(&verticals, &BTreeMap::new());

    let added = entries
        .iter()
        .find(|entry| entry.key == "fleet-pm-brand-new-v1")
        .expect("the new vertical is in the roster with no source edit");
    assert_eq!(added.name, "PM · Brand New");
    assert_eq!(added.kind, Kind::Child);
    assert!(added.title.contains("Brand New"));
    assert!(!added.is_live());
    assert_eq!(added.pane(), None);
}

#[test]
fn roster_has_one_root_unique_identities_and_parented_children() {
    let (_dir, path) = write_verticals("");
    let entries = from_parts(&load_verticals(&path).unwrap(), &BTreeMap::new());

    let roots: Vec<_> = entries.iter().filter(|e| e.kind == Kind::Root).collect();
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0].key, ORCH_KEY);
    assert_eq!(roots[0].name, "Fleet Orchestrator");

    let keys: std::collections::HashSet<_> = entries.iter().map(|e| e.key.as_str()).collect();
    let names: std::collections::HashSet<_> = entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(keys.len(), entries.len(), "unique keys");
    assert_eq!(names.len(), entries.len(), "unique names");
    for entry in entries.iter().filter(|entry| entry.kind == Kind::Child) {
        assert_eq!(
            route_key(entry).unwrap(),
            rakazo_beeper::fleet::BotKey::pm(entry.vertical().unwrap())
        );
        assert!(entry.key.starts_with("fleet-pm-") && entry.key.ends_with("-v1"));
        assert!(entry.name.starts_with("PM · "));
    }
}

#[test]
fn paused_vertical_is_marked_visible_and_never_mapped_to_orch() {
    let (_dir, path) = write_verticals("");
    let entries = from_parts(&load_verticals(&path).unwrap(), &BTreeMap::new());
    let transmog = entries
        .iter()
        .find(|entry| entry.key == "fleet-pm-transmog-v1")
        .expect("paused vertical stays in the roster");
    assert!(transmog.paused);
    assert_eq!(transmog.kind, Kind::Child);
    assert_eq!(transmog.vertical(), Some("transmog"));
    assert_ne!(transmog.key, ORCH_KEY);
    assert_ne!(transmog.name, "Fleet Orchestrator");
}

#[test]
fn live_state_joins_every_live_pm_and_marks_the_rest_offline() {
    let (_dir, path) = write_verticals("");
    let live = parse_live_tsv(
        "fpl\tpm-fpl\t%40\tlive:node\treview\t1\tunibus\t0\n\
         marbles\tpm-marbles\t%31\tlive:node\trunning\t2\t-\t1\n",
    );
    let entries = from_parts(&load_verticals(&path).unwrap(), &live);

    let fpl = entries.iter().find(|e| e.key == "fleet-pm-fpl-v1").unwrap();
    let marbles = entries
        .iter()
        .find(|e| e.key == "fleet-pm-marbles-v1")
        .unwrap();
    assert_eq!(fpl.pane(), Some("%40"));
    assert_eq!(marbles.pane(), Some("%31"));
    assert_eq!(marbles.live.as_ref().unwrap().workers, 2);

    // The paused vertical has no live row: offline, not absent and not invented.
    let transmog = entries
        .iter()
        .find(|e| e.key == "fleet-pm-transmog-v1")
        .unwrap();
    assert!(!transmog.is_live());
    assert_eq!(transmog.pane(), None);

    // A live row for an unconfigured vertical does not invent a roster entry.
    let live = parse_live_tsv("ghost\tpm-ghost\t%99\tlive:node\tidle\t0\t-\t0\n");
    let entries = from_parts(&load_verticals(&path).unwrap(), &live);
    assert!(entries.iter().all(|entry| entry.key != "fleet-pm-ghost-v1"));
}

#[test]
fn malformed_live_rows_are_skipped_not_guessed() {
    let live = parse_live_tsv("not-enough\n\nfpl\tpm-fpl\n");
    assert!(live.is_empty());
}
