//! Accessibility-tree layer (AT-SPI over D-Bus).
//!
//! `snapshot` reads the tree of every app on an accessibility bus and gives
//! each node a ref such as `s3:e12`. Refs belong to the snapshot that produced
//! them; a newer snapshot invalidates older refs. Actions on a ref are semantic
//! AT-SPI calls (`Action.DoAction`, `Component.GrabFocus`) and never move the
//! pointer. Pixel tools accept a ref too and click the node's centre with the
//! ordinary injector.
//!
//! Pure functions (formatting, ref parsing, interactivity filter, sway window
//! offsets, centre) live here so they can be unit-tested without a bus. The
//! D-Bus walker is exercised only on the real machine.
//!
//! Coordinates: GTK4 on Wayland reports every `CoordType::SCREEN` extent as 0,
//! so the walker asks for `CoordType::WINDOW` and adds the window's position
//! on the output. The sandbox knows that position from sway IPC; the real
//! GNOME desktop does not expose it, so desktop nodes carry no rect.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};

/// Node rectangle in screenshot pixels of the target (origin top-left).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Where a node lives on the bus, enough to call methods on it later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handle {
    pub bus_name: String,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeStates {
    pub showing: bool,
    pub focused: bool,
    pub checked: bool,
    pub editable: bool,
    pub sensitive: bool,
}

impl Default for NodeStates {
    /// A node that was never queried is treated as sensitive: `sensitive ==
    /// false` means disabled, which needs evidence from the accessibility bus.
    fn default() -> Self {
        Self {
            showing: false,
            focused: false,
            checked: false,
            editable: false,
            sensitive: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// 1-based position in the snapshot; the `e` number of the ref.
    pub index: usize,
    /// Depth below the application node (window/frame = 0).
    pub depth: usize,
    pub pid: u32,
    /// Localised role name as AT-SPI reports it (`push button`, `text`, ...).
    pub role: String,
    pub name: String,
    /// None when the position on the screenshot is unknown (desktop target).
    pub rect: Option<Rect>,
    /// Action names from `Action.GetActions`, in interface order.
    pub actions: Vec<String>,
    /// Text content of text-like nodes, already truncated to `TEXT_PREVIEW_CHARS`.
    pub text: Option<String>,
    pub states: NodeStates,
    pub handle: Handle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    pub pid: u32,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub id: u32,
    pub apps: Vec<AppInfo>,
    pub nodes: Vec<Node>,
    /// The walk stopped at `max_nodes` before the tree was exhausted.
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotOptions {
    /// Keep only interactive nodes and the containers needed to read them.
    pub interactive_only: bool,
    /// Upper bound on nodes kept, counted after filtering.
    pub max_nodes: usize,
    /// Restrict to apps whose name contains this (case-insensitive).
    pub app: Option<String>,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            interactive_only: true,
            max_nodes: DEFAULT_MAX_NODES,
            app: None,
        }
    }
}

pub const DEFAULT_MAX_NODES: usize = 300;
pub const WALK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Bound for a single D-Bus call made outside the walk (connect, DoAction,
/// GrabFocus): an app that does not answer must not wedge the server.
pub const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
pub const TEXT_PREVIEW_CHARS: usize = 200;
pub const NAME_PREVIEW_CHARS: usize = 80;

/// Roles that always count as interactive, by AT-SPI role name.
pub const INTERACTIVE_ROLES: &[&str] = &[
    "push button",
    "toggle button",
    "button",
    "check box",
    "radio button",
    "menu item",
    "check menu item",
    "radio menu item",
    "menu",
    "menu button",
    "combo box",
    "text",
    "text box",
    "entry",
    "password text",
    "spin button",
    "slider",
    "scroll bar",
    "link",
    "list item",
    "tree item",
    "page tab",
    "tab",
    "table cell",
    "switch",
];

/// Containers kept in interactive snapshots so the reader knows which window
/// or dialog a node belongs to.
pub const CONTAINER_ROLES: &[&str] = &[
    "application",
    "frame",
    "window",
    "dialog",
    "alert",
    "file chooser",
];

/// A node is interactive when its role is in `INTERACTIVE_ROLES`, or it
/// exposes a plain action (one without a dot: `click`, `press`, `activate`,
/// `toggle`, `expand`; GTK4 gives every label dotted actions such as
/// `clipboard.copy`, which do not count).
pub fn is_interactive(node: &Node) -> bool {
    INTERACTIVE_ROLES.contains(&node.role.as_str()) || !plain_actions(&node.actions).is_empty()
}

/// Plain (undotted) action names, in interface order.
pub fn plain_actions(actions: &[String]) -> Vec<&str> {
    actions
        .iter()
        .map(String::as_str)
        .filter(|name| !name.contains('.'))
        .collect()
}

/// GTK4 menu items, list rows and some buttons have no accessible name of
/// their own; the text sits in a child label. Give such an interactive node
/// the name of its first named descendant label (document order, any depth
/// below it, stopping at the next sibling) so the snapshot line is readable.
/// Nodes that already have a name are left alone.
pub fn fill_names_from_labels(nodes: &mut [Node]) {
    for position in 0..nodes.len() {
        if !nodes[position].name.is_empty() || !is_interactive(&nodes[position]) {
            continue;
        }
        let depth = nodes[position].depth;
        let mut found = None;
        for candidate in &nodes[position + 1..] {
            if candidate.depth <= depth {
                break;
            }
            if candidate.role == "label" && !candidate.name.is_empty() {
                found = Some(candidate.name.clone());
                break;
            }
        }
        if let Some(name) = found {
            nodes[position].name = name;
        }
    }
}

/// Decide which nodes of a raw walk to keep, in document order.
///
/// Expected results:
/// - `interactive_only=false` keeps every node that is `showing`; nodes that
///   are not showing are dropped in both modes.
/// - `interactive_only=true` keeps nodes where `is_interactive` holds, plus
///   `CONTAINER_ROLES` nodes, plus any *named* ancestor of a kept node (so the
///   output indents under a window, tab or group the reader can recognise);
///   unnamed wrappers such as GTK4's `generic` chains, other labels, panels
///   and fillers are dropped.
/// - `app` filters by case-insensitive substring of the app name; apps that do
///   not match contribute no nodes.
/// - After filtering, at most `max_nodes` nodes remain and `truncated` is set
///   when some were cut. Re-numbering does not happen: `index` keeps the
///   position in the raw walk so refs stay stable across filters.
pub fn filter_nodes(
    apps: &[AppInfo],
    nodes: Vec<Node>,
    options: &SnapshotOptions,
) -> (Vec<Node>, bool) {
    let matching_pids: Option<HashSet<u32>> = options.app.as_ref().map(|filter| {
        let needle = filter.to_lowercase();
        apps.iter()
            .filter(|app| app.name.to_lowercase().contains(&needle))
            .map(|app| app.pid)
            .collect()
    });
    let app_matches = |node: &Node| {
        matching_pids
            .as_ref()
            .is_none_or(|pids| pids.contains(&node.pid))
    };

    // First pass: mark every ancestor of a node that is kept on its own merits
    // so the surviving children still indent under a visible parent.
    let mut needed = vec![false; nodes.len()];
    let mut path: Vec<usize> = Vec::new();
    for (position, node) in nodes.iter().enumerate() {
        if path.len() > node.depth {
            path.truncate(node.depth);
        }
        while path.len() < node.depth {
            path.push(usize::MAX);
        }
        path.push(position);

        let keeps_subtree = node.states.showing
            && app_matches(node)
            && (is_interactive(node) || CONTAINER_ROLES.contains(&node.role.as_str()));
        if keeps_subtree {
            for ancestor in &path[..node.depth] {
                if *ancestor != usize::MAX {
                    needed[*ancestor] = true;
                }
            }
        }
    }

    let mut kept = Vec::new();
    for (position, node) in nodes.into_iter().enumerate() {
        if !node.states.showing || !app_matches(&node) {
            continue;
        }
        let keep = if options.interactive_only {
            is_interactive(&node)
                || CONTAINER_ROLES.contains(&node.role.as_str())
                || (needed[position] && !node.name.is_empty())
        } else {
            true
        };
        if keep {
            kept.push(node);
        }
    }

    let truncated = kept.len() > options.max_nodes;
    kept.truncate(options.max_nodes);
    (kept, truncated)
}

/// Render a snapshot as compact text for the model.
///
/// Expected format (one node per line, two spaces per depth level after the
/// first kept ancestor, so depth is relative to kept nodes, not the raw tree):
///
/// ```text
/// snapshot s3 target=sandbox apps=1 nodes=5 (interactive only; interactive=false shows every node)
/// app gnome-text-editor pid=498595
/// s3:e1 frame "Hello (草稿) — 文字編輯器" (0,0 1920x1080)
///   s3:e4 push button "開啟(O)" (6,6 84x34) [click]
///   s3:e9 text "" (0,99 1920x981) editable focused text="Hello from wayhand-mcp!"
///   s3:e12 check box "Wrap" (10,40 20x20) [toggle] checked
/// (output truncated: max_nodes=300 reached, or the raw walk budget or the 15 s time limit ran out; narrow with app=..., or raise max_nodes)
/// ```
///
/// Rules:
/// - `(x,y WxH)` is omitted when `rect` is None; the header then says
///   `positions=unknown (desktop target; use activate with refs)`.
/// - `[...]` lists `plain_actions` only; omitted when empty.
/// - State words appear only when true: `editable`, `focused`, `checked`;
///   `disabled` appears when `sensitive` is false.
/// - `text="..."` appears when `text` is Some; it is already truncated, and
///   embedded newlines are rendered as `\n`.
/// - Names longer than `NAME_PREVIEW_CHARS` are cut with `…`.
/// - A snapshot with zero nodes renders the header, the app lines and then
///   `no nodes: the app may still be starting (snapshot again in a moment), or it is not on the accessibility bus (GTK apps must start after sandbox_start; Qt needs QT_ACCESSIBILITY=1)`.
///   When `options.app` is set it says instead
///   `no app name contains "<filter>"; apps on the bus: a, b` (or `none`).
pub fn format_snapshot(
    snapshot: &Snapshot,
    target_name: &str,
    options: &SnapshotOptions,
) -> String {
    let mode = if options.interactive_only {
        "interactive only; interactive=false shows every node"
    } else {
        "all showing nodes; interactive=true keeps only interactive ones"
    };
    let mut header = format!(
        "snapshot s{} target={} apps={} nodes={} ({mode})",
        snapshot.id,
        target_name,
        snapshot.apps.len(),
        snapshot.nodes.len()
    );
    if !snapshot.nodes.is_empty() && snapshot.nodes.iter().all(|node| node.rect.is_none()) {
        header.push_str(" positions=unknown (desktop target; use activate with refs)");
    }

    let mut lines = vec![header];
    for app in &snapshot.apps {
        lines.push(format!("app {} pid={}", app.name, app.pid));
    }

    if snapshot.nodes.is_empty()
        && let Some(filter) = &options.app
    {
        let names: Vec<&str> = snapshot.apps.iter().map(|app| app.name.as_str()).collect();
        lines.push(format!(
            "no app name contains {filter:?}; apps on the bus: {}",
            if names.is_empty() {
                "none".to_owned()
            } else {
                names.join(", ")
            }
        ));
    } else if snapshot.nodes.is_empty() {
        lines.push(
            "no nodes: the app may still be starting (snapshot again in a moment), or it is not on the accessibility bus (GTK apps must start after sandbox_start; Qt needs QT_ACCESSIBILITY=1)"
                .to_owned(),
        );
    } else {
        // Depth relative to the kept nodes, not to the raw tree: hidden
        // intermediate nodes leave gaps in `depth` that must not become blank
        // indentation levels.
        let mut kept_depths: Vec<usize> = Vec::new();
        for node in &snapshot.nodes {
            while kept_depths.last().is_some_and(|depth| *depth >= node.depth) {
                kept_depths.pop();
            }
            let mut line = "  ".repeat(kept_depths.len());
            kept_depths.push(node.depth);

            line.push_str(&format!(
                "s{}:e{} {} \"{}\"",
                snapshot.id,
                node.index,
                node.role,
                escape_field(&shorten_name(&node.name))
            ));
            if let Some(rect) = node.rect {
                line.push_str(&format!(
                    " ({},{} {}x{})",
                    rect.x, rect.y, rect.width, rect.height
                ));
            }
            let actions = plain_actions(&node.actions);
            if !actions.is_empty() {
                line.push_str(&format!(" [{}]", actions.join(" ")));
            }
            if node.states.editable {
                line.push_str(" editable");
            }
            if node.states.focused {
                line.push_str(" focused");
            }
            if node.states.checked {
                line.push_str(" checked");
            }
            if !node.states.sensitive {
                line.push_str(" disabled");
            }
            if let Some(text) = &node.text {
                line.push_str(&format!(" text=\"{}\"", escape_field(text)));
            }
            lines.push(line);
        }
    }

    if snapshot.truncated {
        lines.push(format!(
            "(output truncated: max_nodes={} reached, or the raw walk budget or the 15 s time limit ran out; narrow with app=..., or raise max_nodes)",
            options.max_nodes
        ));
    }

    lines.join("\n")
}

fn shorten_name(name: &str) -> String {
    if name.chars().count() <= NAME_PREVIEW_CHARS {
        return name.to_owned();
    }
    let mut shortened: String = name.chars().take(NAME_PREVIEW_CHARS).collect();
    shortened.push('\u{2026}');
    shortened
}

fn escape_field(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Parse `s3:e12` into (snapshot id 3, node index 12).
///
/// Expected results: accepts `s3:e12`; rejects empty, `e12`, `s3`, `s3:12`,
/// `sx:e1`, `s3:e0` (indices start at 1) with an error that quotes the input
/// and shows the expected form.
pub fn parse_ref(text: &str) -> Result<(u32, usize), String> {
    let invalid =
        || format!("invalid ref \"{text}\": expected s<snapshot>:e<node> (for example s3:e12)");
    let Some(rest) = text.strip_prefix('s') else {
        return Err(invalid());
    };
    let Some((snapshot_text, node_text)) = rest.split_once(':') else {
        return Err(invalid());
    };
    let Some(node_text) = node_text.strip_prefix('e') else {
        return Err(invalid());
    };
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(snapshot_text) || !digits(node_text) {
        return Err(invalid());
    }
    let snapshot_id = snapshot_text.parse::<u32>().map_err(|_| invalid())?;
    let node_index = node_text.parse::<usize>().map_err(|_| invalid())?;
    if node_index == 0 {
        return Err(invalid());
    }
    Ok((snapshot_id, node_index))
}

/// Look a ref up in the most recent snapshot of the target.
///
/// Expected results:
/// - `None` snapshot: error `no snapshot for target X; call snapshot first`.
/// - Snapshot id differs: error naming both ids and the target, telling the
///   caller to call `snapshot` again (`stale ref s2:e5: current sandbox snapshot is s3; call snapshot again`).
/// - Index not present among the snapshot's nodes: error `unknown ref s3:e99`.
/// - Otherwise the node.
pub fn resolve_ref<'a>(
    snapshot: Option<&'a Snapshot>,
    text: &str,
    target_name: &str,
) -> Result<&'a Node, String> {
    let Some(snapshot) = snapshot else {
        return Err(format!(
            "no snapshot for target {target_name}; call snapshot first"
        ));
    };
    let (snapshot_id, node_index) = parse_ref(text)?;
    if snapshot_id != snapshot.id {
        return Err(format!(
            "stale ref {text}: current {target_name} snapshot is s{}; call snapshot again",
            snapshot.id
        ));
    }
    snapshot
        .nodes
        .iter()
        .find(|node| node.index == node_index)
        .ok_or_else(|| format!("unknown ref {text}"))
}

/// Centre of a rect in screenshot pixels, clamped to be inside the rect.
/// For a 0-width or 0-height rect return its origin.
pub fn center(rect: Rect) -> (i64, i64) {
    if rect.width <= 0 || rect.height <= 0 {
        return (i64::from(rect.x), i64::from(rect.y));
    }
    (
        i64::from(rect.x) + i64::from(rect.width) / 2,
        i64::from(rect.y) + i64::from(rect.height) / 2,
    )
}

/// One toplevel from `swaymsg -t get_tree`: title and client-area rect in
/// output pixels (`rect` is the container with borders, `window_rect` the
/// client area relative to it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwayWindow {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Parse `swaymsg -t get_tree` JSON into `pid -> toplevels` in document order.
/// Nodes without a `pid` are skipped but their descendants are still visited.
pub fn sway_windows(tree_json: &str) -> Result<HashMap<u32, Vec<SwayWindow>>, String> {
    let tree: serde_json::Value = serde_json::from_str(tree_json)
        .map_err(|error| format!("invalid sway tree JSON: {error}"))?;
    let mut windows = HashMap::new();
    collect_sway_windows(&tree, &mut windows);
    Ok(windows)
}

/// Pick the sway toplevel an AT-SPI window node belongs to: by title, else by
/// client size, else the only window of that pid. None when ambiguous, so a
/// dialog never inherits the main window's offset.
pub fn match_sway_window<'a>(
    windows: &'a [SwayWindow],
    title: &str,
    size: Option<(i32, i32)>,
) -> Option<&'a SwayWindow> {
    if let Some(found) = windows
        .iter()
        .find(|w| !title.is_empty() && w.name == title)
    {
        return Some(found);
    }
    if let Some((width, height)) = size {
        let mut by_size = windows
            .iter()
            .filter(|w| w.width == width && w.height == height);
        if let (Some(found), None) = (by_size.next(), by_size.next()) {
            return Some(found);
        }
    }
    match windows {
        [only] => Some(only),
        _ => None,
    }
}

fn collect_sway_windows(node: &serde_json::Value, windows: &mut HashMap<u32, Vec<SwayWindow>>) {
    if let Some(pid) = node
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        && let (Some(x), Some(y), Some(window_x), Some(window_y)) = (
            json_i32(node, "/rect/x"),
            json_i32(node, "/rect/y"),
            json_i32(node, "/window_rect/x"),
            json_i32(node, "/window_rect/y"),
        )
    {
        windows.entry(pid).or_default().push(SwayWindow {
            name: node
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            x: x.saturating_add(window_x),
            y: y.saturating_add(window_y),
            width: json_i32(node, "/window_rect/width").unwrap_or(0),
            height: json_i32(node, "/window_rect/height").unwrap_or(0),
        });
    }

    for key in ["nodes", "floating_nodes"] {
        if let Some(children) = node.get(key).and_then(serde_json::Value::as_array) {
            for child in children {
                collect_sway_windows(child, windows);
            }
        }
    }
}

fn json_i32(value: &serde_json::Value, pointer: &str) -> Option<i32> {
    i32::try_from(value.pointer(pointer)?.as_i64()?).ok()
}

/// Interfaces and constants of the AT-SPI2 D-Bus protocol used below.
const ACCESSIBLE_INTERFACE: &str = "org.a11y.atspi.Accessible";
const COMPONENT_INTERFACE: &str = "org.a11y.atspi.Component";
const ACTION_INTERFACE: &str = "org.a11y.atspi.Action";
const TEXT_INTERFACE: &str = "org.a11y.atspi.Text";
const REGISTRY_BUS_NAME: &str = "org.a11y.atspi.Registry";
const REGISTRY_ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
const DBUS_BUS_NAME: &str = "org.freedesktop.DBus";
const DBUS_PATH: &str = "/org/freedesktop/DBus";
const DBUS_INTERFACE: &str = "org.freedesktop.DBus";
/// `AtspiCoordType`: 0 is SCREEN (all zeros on Wayland), 1 is WINDOW.
const WINDOW_COORD_TYPE: u32 = 1;
/// Roles whose content is read through the Text interface. `password text`
/// is deliberately absent.
const TEXT_ROLES: &[&str] = &["text", "text box", "entry", "label", "heading", "paragraph"];

// AtspiStateType bit numbers for the two `au` words returned by GetState.
const STATE_CHECKED: u32 = 4;
const STATE_EDITABLE: u32 = 7;
const STATE_FOCUSED: u32 = 12;
const STATE_SENSITIVE: u32 = 24;
const STATE_SHOWING: u32 = 25;

#[derive(Default)]
struct WalkState {
    apps: Vec<AppInfo>,
    nodes: Vec<Node>,
    truncated: bool,
}

struct Pending {
    handle: Handle,
    depth: usize,
    /// Screen offset of the window this node belongs to, once known.
    offset: Option<(i32, i32)>,
}

/// Live connection to one accessibility bus.
pub struct Bus {
    connection: zbus::Connection,
}

impl Bus {
    /// Connect to the bus at a D-Bus address (`unix:path=...`), used for the
    /// sandbox's private bus.
    pub async fn connect_address(address: &str) -> Result<Self> {
        let connection = tokio::time::timeout(
            CALL_TIMEOUT,
            zbus::connection::Builder::address(address)
                .context("parsing the accessibility bus address")?
                .build(),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "the accessibility bus did not accept the connection within {CALL_TIMEOUT:?}"
            )
        })?
        .context("connecting to the accessibility bus")?;
        Ok(Self { connection })
    }

    /// Connect to the session's accessibility bus: ask `org.a11y.Bus` on the
    /// session bus for `GetAddress`, then connect there. When the socket does
    /// not exist although an address was returned, the error must say the
    /// accessibility bus socket is missing and that logging out and back in
    /// restores it.
    pub async fn connect_session() -> Result<Self> {
        let session = tokio::time::timeout(CALL_TIMEOUT, zbus::Connection::session())
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "the D-Bus session bus did not accept the connection within {CALL_TIMEOUT:?}"
                )
            })?
            .context("connecting to the D-Bus session bus")?;
        let proxy = zbus::Proxy::new(&session, "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus")
            .await
            .context("creating the org.a11y.Bus proxy on the session bus")?;
        let address: String = proxy.call("GetAddress", &()).await.with_context(
            || "asking org.a11y.Bus for the accessibility bus address (is at-spi2-core installed?)",
        )?;

        if let Some(path) = address
            .split(',')
            .next()
            .and_then(|part| part.strip_prefix("unix:path="))
            && !std::path::Path::new(path).exists()
        {
            anyhow::bail!(
                "the accessibility bus socket {path} is missing; logging out and back in restores it"
            );
        }

        // The session may answer with the GUID of a daemon that has since been
        // replaced while the socket path is current; zbus rejects a GUID
        // mismatch, so connect by path only.
        let address = address
            .split(',')
            .filter(|part| !part.starts_with("guid="))
            .collect::<Vec<_>>()
            .join(",");
        Self::connect_address(&address).await
    }

    /// Walk every application on the bus and build the raw node list
    /// (document order, depth-first). Every app on the bus is listed in
    /// `apps`; only apps matching `options.app` are walked. `windows` lists
    /// each pid's sway toplevels; None (desktop target) means positions are
    /// unknown and no node gets a rect. Each depth-0 node (a window) is
    /// matched to one sway toplevel with `match_sway_window` and its
    /// descendants inherit that offset; rects are
    /// `Component.GetExtents(WINDOW)` plus the offset, or None when no
    /// toplevel matched.
    /// The whole walk is bounded by `WALK_TIMEOUT`; on expiry the nodes
    /// collected so far are returned with `truncated = true`.
    ///
    /// The walk stops collecting (and marks truncated) once `raw_limit` nodes
    /// were visited so a huge app cannot stall the tool; `raw_limit` is
    /// `options.max_nodes * 8` capped at 5000. Nodes that are not `showing`
    /// are recorded but their subtrees are skipped (AT-SPI defines SHOWING
    /// as "this and every ancestor is visible").
    /// Text roles (`text`, `entry`, `password text` is skipped, `label`,
    /// `heading`, `paragraph`) get `text` from `Text.GetText(0, min(count, TEXT_PREVIEW_CHARS))`.
    pub async fn walk(
        &self,
        options: &SnapshotOptions,
        windows: Option<&HashMap<u32, Vec<SwayWindow>>>,
    ) -> Result<(Vec<AppInfo>, Vec<Node>, bool)> {
        let mut state = WalkState::default();
        match tokio::time::timeout(WALK_TIMEOUT, self.walk_apps(options, windows, &mut state)).await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => state.truncated = true,
        }
        fill_names_from_labels(&mut state.nodes);
        Ok((state.apps, state.nodes, state.truncated))
    }

    /// Run a semantic action on a node. `action` None picks the first plain
    /// action; when the node has no plain action the error lists every
    /// action (dotted ones included) so the caller can opt in with a name,
    /// and suggests click or type when there are none at all. Some(name) must
    /// match one of the node's actions (ASCII case-insensitive), otherwise the error lists the
    /// available names. Returns the name of the action performed; an AT-SPI
    /// `false` reply is an error saying the app refused the action.
    pub async fn do_action(
        &self,
        handle: &Handle,
        actions: &[String],
        action: Option<&str>,
    ) -> Result<String> {
        let chosen = match action {
            Some(name)
                if let Some(found) = actions
                    .iter()
                    .find(|available| available.eq_ignore_ascii_case(name)) =>
            {
                found.clone()
            }
            Some(_) if actions.is_empty() => {
                anyhow::bail!(
                    "{} has no actions; use click with the ref, or type, instead",
                    describe_handle(handle)
                )
            }
            Some(name) => {
                anyhow::bail!(
                    "{} has no action {name:?}; available actions: {}",
                    describe_handle(handle),
                    actions.join(", ")
                )
            }
            None => match plain_actions(actions).first() {
                Some(name) => (*name).to_owned(),
                None if actions.is_empty() => {
                    anyhow::bail!(
                        "{} has no actions; use click with the ref, or type, instead",
                        describe_handle(handle)
                    )
                }
                None => {
                    anyhow::bail!(
                        "{} has no plain action; available actions: {}; pass action=<name> to pick one",
                        describe_handle(handle),
                        actions.join(", ")
                    )
                }
            },
        };

        let index = actions
            .iter()
            .position(|available| available == &chosen)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "action {chosen:?} is no longer available on {}",
                    describe_handle(handle)
                )
            })?;
        let proxy = self.proxy(handle, ACTION_INTERFACE).await?;
        let performed: bool =
            tokio::time::timeout(CALL_TIMEOUT, proxy.call("DoAction", &(index as i32,)))
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "the application did not answer Action.DoAction within {CALL_TIMEOUT:?}"
                    )
                })?
                .with_context(|| {
                    format!(
                        "calling Action.DoAction({index}) ({chosen:?}) on {}",
                        describe_handle(handle)
                    )
                })?;
        if !performed {
            anyhow::bail!(
                "the application refused action {chosen:?} on {}",
                describe_handle(handle)
            );
        }
        Ok(chosen)
    }

    /// `Component.GrabFocus`; error when the node has no Component interface
    /// or the app refused.
    pub async fn grab_focus(&self, handle: &Handle) -> Result<()> {
        let proxy = self.proxy(handle, COMPONENT_INTERFACE).await?;
        let focused: bool = tokio::time::timeout(CALL_TIMEOUT, proxy.call("GrabFocus", &()))
            .await
            .map_err(|_| {
                anyhow::anyhow!("the application did not answer GrabFocus within {CALL_TIMEOUT:?}")
            })?
            .with_context(|| {
                format!(
                    "GrabFocus on {} failed (the node has no Component interface, or it is gone)",
                    describe_handle(handle)
                )
            })?;
        if !focused {
            anyhow::bail!(
                "the application refused GrabFocus for {}",
                describe_handle(handle)
            );
        }
        Ok(())
    }

    /// Dynamic proxy for one node and one AT-SPI interface.
    async fn proxy(
        &self,
        handle: &Handle,
        interface: &'static str,
    ) -> Result<zbus::Proxy<'static>> {
        zbus::proxy::Builder::new(&self.connection)
            .destination(handle.bus_name.clone())
            .and_then(|builder| builder.path(handle.path.clone()))
            .and_then(|builder| builder.interface(interface))
            .map(|builder| builder.cache_properties(zbus::proxy::CacheProperties::No))
            .with_context(|| format!("naming a {interface} proxy for {}", handle.path))?
            .build()
            .await
            .with_context(|| format!("creating a {interface} proxy for {}", handle.path))
    }

    /// Direct children of an accessible. A child with an empty bus name lives
    /// on the same connection as its parent, per the AT-SPI convention.
    async fn children(&self, handle: &Handle) -> Result<Vec<Handle>> {
        let proxy = self.proxy(handle, ACCESSIBLE_INTERFACE).await?;
        let children: Vec<(String, zbus::zvariant::OwnedObjectPath)> = proxy
            .call("GetChildren", &())
            .await
            .with_context(|| format!("calling Accessible.GetChildren on {}", handle.path))?;
        Ok(children
            .into_iter()
            .map(|(bus_name, path)| Handle {
                bus_name: if bus_name.is_empty() {
                    handle.bus_name.clone()
                } else {
                    bus_name
                },
                path: path.to_string(),
            })
            .collect())
    }

    /// Name of an accessible, or an empty string when the node is gone.
    async fn node_name(&self, handle: &Handle) -> String {
        match self.proxy(handle, ACCESSIBLE_INTERFACE).await {
            Ok(proxy) => proxy
                .get_property::<String>("Name")
                .await
                .unwrap_or_default(),
            Err(_) => String::new(),
        }
    }

    /// PID owning a bus name, asked on the accessibility bus itself.
    async fn process_id(&self, bus_name: &str) -> Result<u32> {
        let proxy = zbus::Proxy::new_owned(
            self.connection.clone(),
            DBUS_BUS_NAME.to_owned(),
            DBUS_PATH.to_owned(),
            DBUS_INTERFACE.to_owned(),
        )
        .await
        .context("creating the org.freedesktop.DBus proxy on the accessibility bus")?;
        proxy
            .call("GetConnectionUnixProcessID", &(bus_name,))
            .await
            .with_context(|| format!("asking the bus for the pid of {bus_name}"))
    }

    /// Depth-first pre-order walk of every application. Individual node errors
    /// degrade to empty fields; only the registry itself is fatal.
    async fn walk_apps(
        &self,
        options: &SnapshotOptions,
        windows: Option<&HashMap<u32, Vec<SwayWindow>>>,
        state: &mut WalkState,
    ) -> Result<()> {
        let registry = Handle {
            bus_name: REGISTRY_BUS_NAME.to_owned(),
            path: REGISTRY_ROOT_PATH.to_owned(),
        };
        let applications = self
            .children(&registry)
            .await
            .with_context(|| "reading the applications registered on the accessibility bus")?;
        let raw_limit = options.max_nodes.saturating_mul(8).min(5000);
        let app_filter = options.app.as_ref().map(|filter| filter.to_lowercase());

        for application in applications {
            let app_name = self.node_name(&application).await;
            let pid = self.process_id(&application.bus_name).await.unwrap_or(0);
            let matches = app_filter
                .as_ref()
                .is_none_or(|filter| app_name.to_lowercase().contains(filter));
            state.apps.push(AppInfo {
                pid,
                name: app_name,
            });
            if !matches {
                continue;
            }

            let children = match self.children(&application).await {
                Ok(children) => children,
                Err(error) => {
                    tracing::debug!(
                        error = %error,
                        path = %application.path,
                        "skipping an application whose children could not be read"
                    );
                    continue;
                }
            };

            let app_windows: &[SwayWindow] = windows
                .and_then(|map| map.get(&pid))
                .map_or(&[], Vec::as_slice);
            let mut pending: Vec<Pending> = children
                .into_iter()
                .rev()
                .map(|handle| Pending {
                    handle,
                    depth: 0,
                    offset: None,
                })
                .collect();

            while let Some(item) = pending.pop() {
                if state.nodes.len() >= raw_limit {
                    state.truncated = true;
                    break;
                }
                let mut node = self
                    .read_node(&item.handle, state.nodes.len() + 1, item.depth, pid)
                    .await;
                // A window node picks its sway toplevel; descendants inherit it.
                let offset = if item.depth == 0 {
                    let size = node.rect.map(|r| (r.width, r.height));
                    match_sway_window(app_windows, &node.name, size).map(|w| (w.x, w.y))
                } else {
                    item.offset
                };
                node.rect = match (node.rect, offset) {
                    (Some(rect), Some((dx, dy))) => Some(Rect {
                        x: rect.x.saturating_add(dx),
                        y: rect.y.saturating_add(dy),
                        ..rect
                    }),
                    _ => None,
                };
                let showing = node.states.showing;
                state.nodes.push(node);

                // A hidden container's descendants are not worth the calls.
                if showing && let Ok(children) = self.children(&item.handle).await {
                    for child in children.into_iter().rev() {
                        pending.push(Pending {
                            handle: child,
                            depth: item.depth + 1,
                            offset,
                        });
                    }
                }
            }

            if state.truncated {
                break;
            }
        }

        Ok(())
    }

    /// Read one accessible. Never fails: a dead node yields empty fields and
    /// `showing = false`, so the walk keeps going. The rect is window-relative
    /// (`CoordType::WINDOW`); the caller adds the window offset or drops it.
    async fn read_node(&self, handle: &Handle, index: usize, depth: usize, pid: u32) -> Node {
        let mut role = String::new();
        let mut name = String::new();
        let mut states = NodeStates::default();
        if let Ok(proxy) = self.proxy(handle, ACCESSIBLE_INTERFACE).await {
            if let Ok(value) = proxy.call::<_, _, String>("GetRoleName", &()).await {
                role = value;
            }
            if let Ok(value) = proxy.get_property::<String>("Name").await {
                name = value;
            }
            if let Ok(words) = proxy.call::<_, _, Vec<u32>>("GetState", &()).await {
                states = decode_states(&words);
            }
        }

        let mut rect = None;
        if let Ok(component) = self.proxy(handle, COMPONENT_INTERFACE).await
            && let Ok((x, y, width, height)) = component
                .call::<_, _, (i32, i32, i32, i32)>("GetExtents", &(WINDOW_COORD_TYPE,))
                .await
        {
            rect = Some(Rect {
                x,
                y,
                width,
                height,
            });
        }

        let mut actions = Vec::new();
        if let Ok(action) = self.proxy(handle, ACTION_INTERFACE).await
            && let Ok(available) = action
                .call::<_, _, Vec<(String, String, String)>>("GetActions", &())
                .await
        {
            actions = available.into_iter().map(|(name, _, _)| name).collect();
        }

        let mut text = None;
        if TEXT_ROLES.contains(&role.as_str())
            && let Ok(text_proxy) = self.proxy(handle, TEXT_INTERFACE).await
            && let Ok(count) = text_proxy.get_property::<i32>("CharacterCount").await
        {
            let end = count.clamp(0, TEXT_PREVIEW_CHARS as i32);
            if let Ok(value) = text_proxy
                .call::<_, _, String>("GetText", &(0i32, end))
                .await
                && value != name
            {
                text = Some(truncate_chars(&value, TEXT_PREVIEW_CHARS));
            }
        }

        Node {
            index,
            depth,
            pid,
            role,
            name,
            rect,
            actions,
            text,
            states,
            handle: handle.clone(),
        }
    }
}

fn describe_handle(handle: &Handle) -> String {
    format!("accessible {} on {}", handle.path, handle.bus_name)
}

fn decode_states(words: &[u32]) -> NodeStates {
    let bit = |number: u32| {
        words
            .get((number / 32) as usize)
            .is_some_and(|word| word & (1 << (number % 32)) != 0)
    };
    NodeStates {
        showing: bit(STATE_SHOWING),
        focused: bit(STATE_FOCUSED),
        checked: bit(STATE_CHECKED),
        editable: bit(STATE_EDITABLE),
        sensitive: bit(STATE_SENSITIVE),
    }
}

/// Cut `text` to at most `limit` characters without splitting a code point.
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    text.chars().take(limit).collect()
}

#[cfg(test)]
#[path = "a11y_tests.rs"]
mod tests;
