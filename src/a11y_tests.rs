//! Unit tests for the pure functions in `a11y.rs`.

use super::*;

fn actions(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

/// Construct a node with the defaults most tests want: belonging to pid 42,
/// showing, no rect, no actions, no text and no state flags.
fn make_node(index: usize, depth: usize, role: &str, name: &str) -> Node {
    Node {
        index,
        depth,
        pid: 42,
        role: role.to_string(),
        name: name.to_string(),
        rect: None,
        actions: Vec::new(),
        text: None,
        states: NodeStates {
            showing: true,
            ..NodeStates::default()
        },
        handle: Handle {
            bus_name: ":1.42".to_string(),
            path: format!("/org/a11y/atspi/accessible/{index}"),
        },
    }
}

fn with_rect(mut node: Node, x: i32, y: i32, width: i32, height: i32) -> Node {
    node.rect = Some(Rect {
        x,
        y,
        width,
        height,
    });
    node
}

fn with_actions(mut node: Node, names: &[&str]) -> Node {
    node.actions = actions(names);
    node
}

fn with_text(mut node: Node, text: &str) -> Node {
    node.text = Some(text.to_string());
    node
}

fn with_pid(mut node: Node, pid: u32) -> Node {
    node.pid = pid;
    node
}

fn with_states(mut node: Node, states: NodeStates) -> Node {
    node.states = states;
    node
}

fn index_list(nodes: &[Node]) -> Vec<usize> {
    nodes.iter().map(|node| node.index).collect()
}

// ---------------------------------------------------------------------------
// is_interactive
// ---------------------------------------------------------------------------

#[test]
fn is_interactive_is_true_for_roles_in_the_interactive_list() {
    for role in INTERACTIVE_ROLES {
        let node = make_node(1, 0, role, "");
        assert!(
            is_interactive(&node),
            "role {role:?} is listed in INTERACTIVE_ROLES and must be interactive"
        );
    }
}

#[test]
fn is_interactive_is_true_for_an_undotted_action_on_any_role() {
    for action in ["click", "press", "activate", "toggle", "expand"] {
        let node = make_node(1, 0, "label", "some label");
        let node = with_actions(node, &[action]);
        assert!(
            is_interactive(&node),
            "plain action {action:?} makes a label interactive"
        );
    }
}

#[test]
fn is_interactive_is_false_for_a_label_with_only_dotted_actions() {
    let node = with_actions(
        make_node(1, 0, "label", "some label"),
        &["clipboard.copy", "clipboard.paste"],
    );
    assert!(
        !is_interactive(&node),
        "GTK4 labels expose dotted actions such as clipboard.copy; they are not interactive"
    );
}

#[test]
fn is_interactive_is_false_for_a_plain_label_with_no_actions() {
    let node = make_node(1, 0, "label", "plain label");
    assert!(!is_interactive(&node));
}

// ---------------------------------------------------------------------------
// plain_actions
// ---------------------------------------------------------------------------

#[test]
fn plain_actions_keeps_undotted_names_in_interface_order() {
    let names = actions(&[
        "click",
        "clipboard.copy",
        "press",
        "clipboard.paste",
        "toggle",
    ]);
    assert_eq!(plain_actions(&names), vec!["click", "press", "toggle"]);
}

#[test]
fn plain_actions_returns_nothing_when_all_actions_are_dotted_or_empty() {
    assert!(plain_actions(&[]).is_empty());
    let names = actions(&["clipboard.copy", "clipboard.cut"]);
    assert!(plain_actions(&names).is_empty());
}

// ---------------------------------------------------------------------------
// filter_nodes
// ---------------------------------------------------------------------------

/// Document-order fixture used by the interactive/non-interactive filter tests.
///
/// ```text
/// e1  depth 0  frame        (container, no interactive child of its own)
/// e2  depth 1  label        (ancestor of e3/e4, kept only because of them)
/// e3  depth 2  push button  (interactive role)
/// e4  depth 2  text         (interactive role)
/// e5  depth 1  panel        (not interactive, no interactive descendants)
/// e6  depth 2  label        (child of the dropped panel)
/// e7  depth 1  dialog       (container, kept even without interactive children)
/// e8  depth 2  label        (child of the dialog, not kept)
/// e9  depth 1  check box    (interactive role)
/// e10 depth 1  label        (only dotted actions -> not interactive)
/// e11 depth 1  push button  (showing=false -> dropped in both modes)
/// ```
fn filter_fixture() -> (Vec<AppInfo>, Vec<Node>) {
    let apps = vec![AppInfo {
        pid: 42,
        name: "demo".to_string(),
    }];

    let hidden_button = with_states(
        make_node(11, 1, "push button", "Hidden"),
        NodeStates {
            showing: false,
            ..NodeStates::default()
        },
    );

    let nodes = vec![
        make_node(1, 0, "frame", "Main"),
        make_node(2, 1, "label", "Box"),
        make_node(3, 2, "push button", "OK"),
        make_node(4, 2, "text", ""),
        make_node(5, 1, "panel", "Filler"),
        make_node(6, 2, "label", "Panel child"),
        make_node(7, 1, "dialog", "Dialog"),
        make_node(8, 2, "label", "Dialog label"),
        make_node(9, 1, "check box", "Wrap"),
        with_actions(
            make_node(10, 1, "label", "Clipboard label"),
            &["clipboard.copy"],
        ),
        hidden_button,
    ];

    (apps, nodes)
}

#[test]
fn filter_nodes_non_interactive_keeps_every_showing_node() {
    let (apps, nodes) = filter_fixture();
    let options = SnapshotOptions {
        interactive_only: false,
        ..SnapshotOptions::default()
    };

    let (kept, truncated) = filter_nodes(&apps, nodes, &options);

    assert_eq!(index_list(&kept), vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    assert!(!truncated);
}

#[test]
fn filter_nodes_interactive_keeps_interactive_nodes_containers_and_ancestors() {
    let (apps, nodes) = filter_fixture();
    let options = SnapshotOptions {
        interactive_only: true,
        ..SnapshotOptions::default()
    };

    let (kept, truncated) = filter_nodes(&apps, nodes, &options);

    // e3/e4 are interactive; e2 is their non-interactive ancestor. e1 and e7
    // are CONTAINER_ROLES. e5/e6/e8/e10 have no interactive role or action and
    // are not ancestors of anything kept. e11 is hidden.
    assert_eq!(index_list(&kept), vec![1, 2, 3, 4, 7, 9]);
    assert!(!truncated);
}

#[test]
fn filter_nodes_interactive_drops_unnamed_wrapper_ancestors() {
    // GTK4 wraps every widget in unnamed `generic` nodes; they carry no
    // information for the reader and must not survive just because a button
    // sits below them. A named ancestor (the tab panel) still survives.
    let apps = vec![AppInfo {
        pid: 42,
        name: "demo".to_string(),
    }];
    let nodes = vec![
        make_node(1, 0, "application", "Editor"),
        make_node(2, 1, "generic", ""),
        make_node(3, 2, "tab panel", "Draft"),
        make_node(4, 3, "generic", ""),
        make_node(5, 4, "button", "New tab"),
    ];
    let options = SnapshotOptions {
        interactive_only: true,
        ..SnapshotOptions::default()
    };

    let (kept, _) = filter_nodes(&apps, nodes, &options);

    assert_eq!(index_list(&kept), vec![1, 3, 5]);
}

#[test]
fn filter_nodes_drops_hidden_nodes_in_both_modes() {
    let (apps, nodes) = filter_fixture();

    let all = SnapshotOptions {
        interactive_only: false,
        ..SnapshotOptions::default()
    };
    let (kept_all, _) = filter_nodes(&apps, nodes.clone(), &all);
    assert!(
        !index_list(&kept_all).contains(&11),
        "a node with showing=false must be dropped even when interactive_only=false"
    );

    let interactive = SnapshotOptions {
        interactive_only: true,
        ..SnapshotOptions::default()
    };
    let (kept_interactive, _) = filter_nodes(&apps, nodes, &interactive);
    assert!(
        !index_list(&kept_interactive).contains(&11),
        "a node with showing=false must be dropped when interactive_only=true"
    );
}

#[test]
fn filter_nodes_app_filter_is_a_case_insensitive_substring_of_the_app_name() {
    let apps = vec![
        AppInfo {
            pid: 1,
            name: "Gnome Text Editor".to_string(),
        },
        AppInfo {
            pid: 2,
            name: "Firefox".to_string(),
        },
    ];
    let nodes = vec![
        with_pid(make_node(1, 0, "frame", "Editor"), 1),
        with_pid(make_node(2, 1, "text", ""), 1),
        with_pid(make_node(3, 0, "frame", "Browser"), 2),
        with_pid(make_node(4, 1, "push button", "Reload"), 2),
    ];
    let options = SnapshotOptions {
        interactive_only: false,
        max_nodes: DEFAULT_MAX_NODES,
        app: Some("tExT".to_string()),
    };

    let (kept, truncated) = filter_nodes(&apps, nodes, &options);

    assert_eq!(index_list(&kept), vec![1, 2]);
    assert!(!truncated);
}

#[test]
fn filter_nodes_truncates_after_filtering_without_renumbering_indices() {
    let apps = vec![AppInfo {
        pid: 42,
        name: "demo".to_string(),
    }];
    let nodes: Vec<Node> = [10, 20, 30, 40, 50]
        .into_iter()
        .map(|index| make_node(index, 0, "push button", "Button"))
        .collect();
    let options = SnapshotOptions {
        interactive_only: true,
        max_nodes: 2,
        app: None,
    };

    let (kept, truncated) = filter_nodes(&apps, nodes, &options);

    // Indices are the positions in the raw walk, not positions in the result.
    assert_eq!(index_list(&kept), vec![10, 20]);
    assert!(truncated);
}

#[test]
fn filter_nodes_is_not_truncated_when_every_node_fits() {
    let apps = vec![AppInfo {
        pid: 42,
        name: "demo".to_string(),
    }];
    let nodes = vec![
        make_node(1, 0, "push button", "One"),
        make_node(2, 0, "push button", "Two"),
    ];
    let options = SnapshotOptions {
        interactive_only: true,
        max_nodes: 5,
        app: None,
    };

    let (kept, truncated) = filter_nodes(&apps, nodes, &options);

    assert_eq!(index_list(&kept), vec![1, 2]);
    assert!(!truncated);
}

// ---------------------------------------------------------------------------
// format_snapshot
// ---------------------------------------------------------------------------

fn one_app(pid: u32, name: &str) -> Vec<AppInfo> {
    vec![AppInfo {
        pid,
        name: name.to_string(),
    }]
}

#[test]
fn format_snapshot_matches_the_documented_line_format() {
    let snapshot = Snapshot {
        id: 3,
        apps: one_app(498595, "gnome-text-editor"),
        nodes: vec![
            with_rect(
                make_node(1, 0, "frame", "Hello (草稿) — 文字編輯器"),
                0,
                0,
                1920,
                1080,
            ),
            with_actions(
                with_rect(make_node(4, 1, "push button", "開啟(O)"), 6, 6, 84, 34),
                &["click"],
            ),
            with_text(
                with_states(
                    with_rect(make_node(9, 1, "text", ""), 0, 99, 1920, 981),
                    NodeStates {
                        showing: true,
                        focused: true,
                        editable: true,
                        ..NodeStates::default()
                    },
                ),
                "Hello from wayhand-mcp!",
            ),
            with_states(
                with_actions(
                    with_rect(make_node(12, 2, "check box", "Wrap"), 10, 40, 20, 20),
                    &["toggle"],
                ),
                NodeStates {
                    showing: true,
                    checked: true,
                    ..NodeStates::default()
                },
            ),
        ],
        truncated: false,
    };

    let text = format_snapshot(&snapshot, "sandbox", &SnapshotOptions::default());

    let expected = concat!(
        "snapshot s3 target=sandbox apps=1 nodes=4 (interactive only; interactive=false shows every node)\n",
        "app gnome-text-editor pid=498595\n",
        "s3:e1 frame \"Hello (草稿) — 文字編輯器\" (0,0 1920x1080)\n",
        "  s3:e4 push button \"開啟(O)\" (6,6 84x34) [click]\n",
        "  s3:e9 text \"\" (0,99 1920x981) editable focused text=\"Hello from wayhand-mcp!\"\n",
        "    s3:e12 check box \"Wrap\" (10,40 20x20) [toggle] checked",
    );
    assert_eq!(text, expected);
}

#[test]
fn format_snapshot_omits_missing_rects_and_reports_unknown_positions() {
    let snapshot = Snapshot {
        id: 5,
        apps: one_app(42, "demo"),
        nodes: vec![make_node(2, 0, "push button", "OK")],
        truncated: false,
    };

    let text = format_snapshot(&snapshot, "desktop", &SnapshotOptions::default());
    let lines: Vec<&str> = text.lines().collect();

    assert!(
        lines[0].contains("positions=unknown (desktop target; use activate with refs)"),
        "header must explain that desktop nodes carry no positions, got: {:?}",
        lines[0]
    );
    assert_eq!(lines[2], "s5:e2 push button \"OK\"");
}

#[test]
fn format_snapshot_lists_only_plain_actions() {
    let snapshot = Snapshot {
        id: 6,
        apps: one_app(42, "demo"),
        nodes: vec![
            with_actions(
                make_node(1, 0, "label", "Plain"),
                &["clipboard.copy", "click", "press"],
            ),
            with_actions(
                make_node(2, 0, "label", "Dotted"),
                &["clipboard.copy", "clipboard.cut"],
            ),
        ],
        truncated: false,
    };

    let text = format_snapshot(&snapshot, "sandbox", &SnapshotOptions::default());
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(lines[2], "s6:e1 label \"Plain\" [click press]");
    assert_eq!(
        lines[3], "s6:e2 label \"Dotted\"",
        "dotted actions must not appear in the bracket list"
    );
}

#[test]
fn format_snapshot_renders_all_state_words() {
    let snapshot = Snapshot {
        id: 7,
        apps: one_app(42, "demo"),
        nodes: vec![with_states(
            make_node(3, 0, "text", "Body"),
            NodeStates {
                showing: true,
                focused: true,
                checked: true,
                editable: true,
                sensitive: false,
            },
        )],
        truncated: false,
    };

    let text = format_snapshot(&snapshot, "sandbox", &SnapshotOptions::default());
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(
        lines[2],
        "s7:e3 text \"Body\" editable focused checked disabled"
    );
}

#[test]
fn format_snapshot_renders_newlines_in_text_as_a_literal_backslash_n() {
    let snapshot = Snapshot {
        id: 8,
        apps: one_app(42, "demo"),
        nodes: vec![with_text(
            make_node(1, 0, "label", "L"),
            "line one\nline two",
        )],
        truncated: false,
    };

    let text = format_snapshot(&snapshot, "sandbox", &SnapshotOptions::default());
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(
        lines[2], "s8:e1 label \"L\" text=\"line one\\nline two\"",
        "the embedded newline must be escaped, not split into a real line break"
    );
}

#[test]
fn format_snapshot_cuts_long_names_with_an_ellipsis() {
    let long_name = "x".repeat(100);
    let snapshot = Snapshot {
        id: 9,
        apps: one_app(42, "demo"),
        nodes: vec![make_node(1, 0, "label", &long_name)],
        truncated: false,
    };

    let text = format_snapshot(&snapshot, "sandbox", &SnapshotOptions::default());
    let lines: Vec<&str> = text.lines().collect();

    let expected = format!("s9:e1 label \"{}\u{2026}\"", "x".repeat(NAME_PREVIEW_CHARS));
    assert_eq!(lines[2], expected);
}

#[test]
fn format_snapshot_of_an_empty_snapshot_explains_why_it_is_empty() {
    let snapshot = Snapshot {
        id: 7,
        apps: one_app(7, "demo"),
        nodes: Vec::new(),
        truncated: false,
    };

    let text = format_snapshot(&snapshot, "sandbox", &SnapshotOptions::default());

    let expected = concat!(
        "snapshot s7 target=sandbox apps=1 nodes=0 (interactive only; interactive=false shows every node)\n",
        "app demo pid=7\n",
        "no nodes: the app may still be starting (snapshot again in a moment), or it is not on the accessibility bus (GTK apps must start after sandbox_start; Qt needs QT_ACCESSIBILITY=1)",
    );
    assert_eq!(text, expected);
}

#[test]
fn format_snapshot_of_an_empty_filtered_snapshot_names_the_filter_and_the_apps() {
    let snapshot = Snapshot {
        id: 2,
        apps: vec![
            AppInfo {
                pid: 1,
                name: "gnome-text-editor".to_string(),
            },
            AppInfo {
                pid: 2,
                name: "nautilus".to_string(),
            },
        ],
        nodes: Vec::new(),
        truncated: false,
    };
    let options = SnapshotOptions {
        app: Some("firefox".to_string()),
        ..SnapshotOptions::default()
    };

    let text = format_snapshot(&snapshot, "sandbox", &options);

    assert_eq!(
        text.lines().last(),
        Some("no app name contains \"firefox\"; apps on the bus: gnome-text-editor, nautilus")
    );
}

#[test]
fn format_snapshot_appends_the_truncation_footer_with_the_node_limit() {
    let snapshot = Snapshot {
        id: 10,
        apps: one_app(42, "demo"),
        nodes: vec![with_rect(make_node(1, 0, "push button", "OK"), 1, 2, 3, 4)],
        truncated: true,
    };

    let text = format_snapshot(&snapshot, "sandbox", &SnapshotOptions::default());
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(
        lines.last().copied(),
        Some(
            "(output truncated: max_nodes=300 reached, or the raw walk budget or the 15 s time limit ran out; narrow with app=..., or raise max_nodes)"
        )
    );
}

// ---------------------------------------------------------------------------
// parse_ref
// ---------------------------------------------------------------------------

#[test]
fn parse_ref_accepts_snapshot_and_node_ids() {
    assert_eq!(parse_ref("s3:e12"), Ok((3, 12)));
}

#[test]
fn parse_ref_rejects_malformed_refs_and_quotes_the_input() {
    for input in ["", "e12", "s3", "s3:12", "sx:e1", "s3:e0"] {
        let error =
            parse_ref(input).expect_err(&format!("{input:?} must not be accepted as a ref"));
        assert!(
            error.contains(&format!("\"{input}\"")),
            "error for {input:?} must quote the input, got: {error:?}"
        );
        assert!(
            error.contains("s3:e12"),
            "error for {input:?} must show the expected form, got: {error:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// resolve_ref
// ---------------------------------------------------------------------------

fn resolve_snapshot() -> Snapshot {
    Snapshot {
        id: 3,
        apps: one_app(42, "demo"),
        nodes: vec![
            make_node(5, 0, "push button", "OK"),
            make_node(12, 0, "text", ""),
        ],
        truncated: false,
    }
}

#[test]
fn resolve_ref_without_a_snapshot_tells_the_caller_to_snapshot_first() {
    let snapshot: Option<&Snapshot> = None;

    let error = resolve_ref(snapshot, "s3:e1", "sandbox").unwrap_err();

    assert_eq!(error, "no snapshot for target sandbox; call snapshot first");
}

#[test]
fn resolve_ref_rejects_a_ref_from_an_older_snapshot() {
    let snapshot = resolve_snapshot();

    let error = resolve_ref(Some(&snapshot), "s2:e5", "sandbox").unwrap_err();

    assert!(
        error.contains("stale") && error.contains("s2:e5") && error.contains("s3"),
        "error must name the stale ref and the current snapshot id, got: {error:?}"
    );
    assert!(
        error.contains("snapshot"),
        "error must tell the caller to call snapshot again, got: {error:?}"
    );
}

#[test]
fn resolve_ref_rejects_an_unknown_index() {
    let snapshot = resolve_snapshot();

    let error = resolve_ref(Some(&snapshot), "s3:e99", "sandbox").unwrap_err();

    assert_eq!(error, "unknown ref s3:e99");
}

#[test]
fn resolve_ref_returns_the_matching_node() {
    let snapshot = resolve_snapshot();

    let node = resolve_ref(Some(&snapshot), "s3:e12", "sandbox").expect("s3:e12 exists");

    assert_eq!(node.index, 12);
    assert_eq!(node.role, "text");
    assert_eq!(
        node.handle.path, "/org/a11y/atspi/accessible/12",
        "the returned node must be the one from the snapshot, not a re-parsed stub"
    );
}

// ---------------------------------------------------------------------------
// center
// ---------------------------------------------------------------------------

#[test]
fn center_of_an_odd_sized_rect_is_the_integer_middle() {
    let rect = Rect {
        x: 10,
        y: 20,
        width: 5,
        height: 7,
    };

    let (x, y) = center(rect);

    assert_eq!((x, y), (12, 23));
    assert!(x >= i64::from(rect.x) && x < i64::from(rect.x + rect.width));
    assert!(y >= i64::from(rect.y) && y < i64::from(rect.y + rect.height));
}

#[test]
fn center_of_an_even_sized_rect_stays_inside_the_rect() {
    let rect = Rect {
        x: 10,
        y: 20,
        width: 4,
        height: 6,
    };

    let (x, y) = center(rect);

    assert!(
        x >= i64::from(rect.x) && x < i64::from(rect.x + rect.width),
        "centre x {x} must be inside [10, 14)"
    );
    assert!(
        y >= i64::from(rect.y) && y < i64::from(rect.y + rect.height),
        "centre y {y} must be inside [20, 26)"
    );
}

#[test]
fn center_of_a_zero_sized_rect_is_its_origin() {
    assert_eq!(
        center(Rect {
            x: -5,
            y: 7,
            width: 0,
            height: 10,
        }),
        (-5, 7)
    );
    assert_eq!(
        center(Rect {
            x: 3,
            y: 4,
            width: 10,
            height: 0,
        }),
        (3, 4)
    );
}

// ---------------------------------------------------------------------------
// sway_window_offsets
// ---------------------------------------------------------------------------

/// `pid -> (x, y)` of each pid's first toplevel, as the walker's "first
/// window wins" fallback used to compute it.
fn sway_window_offsets(tree_json: &str) -> Result<HashMap<u32, (i32, i32)>, String> {
    Ok(sway_windows(tree_json)?
        .into_iter()
        .filter_map(|(pid, windows)| windows.first().map(|w| (pid, (w.x, w.y))))
        .collect())
}

#[test]
fn sway_window_offsets_adds_container_rect_and_window_rect() {
    // A realistic `swaymsg -t get_tree` shape: root -> output -> workspace ->
    // con nodes. The anonymous con has no pid and must not stop the descent.
    let tree = r#"{
        "id": 1,
        "type": "root",
        "name": "root",
        "rect": { "x": 0, "y": 0, "width": 1920, "height": 1080 },
        "window_rect": { "x": 0, "y": 0, "width": 0, "height": 0 },
        "nodes": [
            {
                "id": 2,
                "type": "output",
                "name": "WL-1",
                "rect": { "x": 0, "y": 0, "width": 1920, "height": 1080 },
                "window_rect": { "x": 0, "y": 0, "width": 0, "height": 0 },
                "nodes": [
                    {
                        "id": 3,
                        "type": "workspace",
                        "name": "1",
                        "rect": { "x": 0, "y": 0, "width": 1920, "height": 1080 },
                        "window_rect": { "x": 0, "y": 0, "width": 0, "height": 0 },
                        "nodes": [
                            {
                                "id": 4,
                                "type": "con",
                                "name": "Editor",
                                "pid": 111,
                                "rect": { "x": 100, "y": 50, "width": 800, "height": 600 },
                                "window_rect": { "x": 2, "y": 3, "width": 796, "height": 597 },
                                "nodes": [
                                    {
                                        "id": 5,
                                        "type": "con",
                                        "name": "anonymous child",
                                        "rect": { "x": 110, "y": 60, "width": 780, "height": 580 },
                                        "window_rect": { "x": 4, "y": 5, "width": 776, "height": 575 },
                                        "nodes": [
                                            {
                                                "id": 6,
                                                "type": "con",
                                                "name": "Nested app",
                                                "pid": 333,
                                                "rect": { "x": 400, "y": 100, "width": 300, "height": 200 },
                                                "window_rect": { "x": 5, "y": 6, "width": 295, "height": 194 },
                                                "nodes": []
                                            }
                                        ]
                                    }
                                ]
                            },
                            {
                                "id": 7,
                                "type": "con",
                                "name": "Editor again",
                                "pid": 111,
                                "rect": { "x": 500, "y": 500, "width": 800, "height": 600 },
                                "window_rect": { "x": 9, "y": 9, "width": 791, "height": 591 },
                                "nodes": []
                            },
                            {
                                "id": 8,
                                "type": "con",
                                "name": "Browser",
                                "pid": 222,
                                "rect": { "x": -10, "y": 20, "width": 300, "height": 200 },
                                "window_rect": { "x": 1, "y": 2, "width": 299, "height": 198 },
                                "nodes": []
                            }
                        ]
                    }
                ]
            }
        ]
    }"#;

    let offsets = sway_window_offsets(tree).expect("the realistic sway tree must parse");

    assert_eq!(
        offsets.get(&111),
        Some(&(102, 53)),
        "first toplevel for pid 111 wins"
    );
    assert_eq!(offsets.get(&222), Some(&(-9, 22)));
    assert_eq!(
        offsets.get(&333),
        Some(&(405, 106)),
        "nodes without a pid are skipped but their descendants are still visited"
    );
    assert_eq!(
        offsets.len(),
        3,
        "only the three pid-bearing toplevels should be recorded"
    );
}

#[test]
fn sway_window_offsets_rejects_invalid_json() {
    assert!(sway_window_offsets("{ this is not json").is_err());
}

// ---------------------------------------------------------------------------
// sway_windows / match_sway_window
// ---------------------------------------------------------------------------

fn sway_tree_two_windows() -> &'static str {
    r#"{"type":"root","rect":{"x":0,"y":0},"window_rect":{"x":0,"y":0},"nodes":[
        {"type":"con","name":"Main (草稿)","pid":111,
         "rect":{"x":0,"y":0,"width":1920,"height":1080},
         "window_rect":{"x":0,"y":0,"width":1920,"height":1080},"nodes":[]}],
      "floating_nodes":[
        {"type":"floating_con","name":"偏好設定","pid":111,
         "rect":{"x":600,"y":300,"width":720,"height":480},
         "window_rect":{"x":0,"y":0,"width":720,"height":480},"nodes":[]}]}"#
}

#[test]
fn sway_windows_keeps_every_toplevel_of_a_pid_with_title_and_size() {
    let windows = sway_windows(sway_tree_two_windows()).expect("parses");
    let list = &windows[&111];
    assert_eq!(list.len(), 2);
    assert_eq!(
        (list[0].name.as_str(), list[0].x, list[0].y),
        ("Main (草稿)", 0, 0)
    );
    assert_eq!(
        (
            list[1].name.as_str(),
            list[1].x,
            list[1].y,
            list[1].width,
            list[1].height
        ),
        ("偏好設定", 600, 300, 720, 480)
    );
}

#[test]
fn match_sway_window_prefers_title_then_size_then_the_only_window() {
    let windows = sway_windows(sway_tree_two_windows()).expect("parses");
    let list = &windows[&111];

    let by_title = match_sway_window(list, "偏好設定", None).expect("title match");
    assert_eq!((by_title.x, by_title.y), (600, 300));

    let by_size = match_sway_window(list, "", Some((720, 480))).expect("size match");
    assert_eq!((by_size.x, by_size.y), (600, 300));

    assert!(
        match_sway_window(list, "", None).is_none(),
        "two windows and nothing to tell them apart must not guess"
    );

    let single = &list[..1];
    let only = match_sway_window(single, "unrelated title", None).expect("single window");
    assert_eq!((only.x, only.y), (0, 0));
}

// ---------------------------------------------------------------------------
// fill_names_from_labels
// ---------------------------------------------------------------------------

#[test]
fn fill_names_from_labels_names_a_menu_item_after_its_child_label_only() {
    let mut nodes = vec![
        make_node(1, 0, "menu", "主選單"),
        with_actions(make_node(2, 1, "menu item", ""), &["Click"]),
        make_node(3, 2, "generic", ""),
        make_node(4, 3, "label", "偏好設定(R)"),
        make_node(5, 3, "label", "Ctrl+,"),
        with_actions(make_node(6, 1, "menu item", ""), &["Click"]),
        make_node(7, 1, "label", "not a child of e6"),
        with_actions(make_node(8, 1, "button", "Named"), &["Click"]),
        make_node(9, 2, "label", "ignored"),
    ];

    fill_names_from_labels(&mut nodes);

    assert_eq!(
        nodes[1].name, "偏好設定(R)",
        "first named descendant label wins"
    );
    assert_eq!(
        nodes[5].name, "",
        "a following sibling label is not a child"
    );
    assert_eq!(nodes[7].name, "Named", "existing names are kept");
    assert_eq!(nodes[0].name, "主選單");
}
