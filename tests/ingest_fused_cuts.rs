//! noetl/ai-meta#561 slice 2: per-event and per-drive server cuts stay
//! flag-gated and default OFF.
//!
//! CI has no database, so the shape is guarded here and the latency effect is
//! measured in kind and prod (see the PR).

const EVENTS_RS: &str = include_str!("../src/handlers/events.rs");
const APP_RS: &str = include_str!("../src/config/app.rs");

fn code_only(src: &str) -> String {
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn both_flags_default_off() {
    let src = code_only(APP_RS);
    assert!(src.contains("ingest_catalog_from_descriptor: false,"));
    assert!(src.contains("playbook_parse_cache: false,"));
}

#[test]
fn descriptor_catalog_id_is_tried_before_the_scan_and_only_under_the_flag() {
    let src = code_only(EVENTS_RS);
    let start = src
        .find("async fn get_catalog_id(")
        .expect("get_catalog_id");
    let body = &src[start..];
    let flag = body
        .find("if state.config.ingest_catalog_from_descriptor {")
        .expect("flag-gated descriptor read");
    let scan = body
        .find("record_event_hotpath_read(site, \"scan\")")
        .expect("scan fallback");
    assert!(flag < scan, "the descriptor read must precede the scan");
}

#[test]
fn every_drive_playbook_load_goes_through_the_gated_helper() {
    let src = code_only(EVENTS_RS);
    assert_eq!(
        src.matches("load_playbook_for_drive(state, catalog_id).await?")
            .count(),
        3,
        "dispatch, in-process trigger and worker apply"
    );
    let helper = &src[src.find("async fn load_playbook_for_drive(").unwrap()..];
    assert!(helper.contains("if state.config.playbook_parse_cache {"));
}
