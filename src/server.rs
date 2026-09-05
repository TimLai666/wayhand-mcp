use std::{collections::HashMap, sync::Arc, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo},
    schemars::JsonSchema,
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};

use crate::{
    calibrate::{self, Calibration},
    coords::Transform,
    inject::{Button, Injector},
    keys::{Combo, KeyMap, KeyStroke, Modifier, TextRun},
    safety::{Budget, BudgetError, settle_duration},
    sandbox::Sandbox,
    screenshot::{self, CapturedScreenshot},
};

pub const COORDINATE_SYSTEM: &str =
    "x,y are pixel coordinates in the most recent screenshot of the same target, origin top-left";
const AUTOMATIC_SCALE_FACTOR_NOTE: &str = "GNOME scaling-factor is 0 (automatic); coordinates are screenshot pixels and do not depend on this value";
const TARGET_DOC: &str = "target: \"sandbox\" (default, recommended) drives a nested sandbox desktop window so the user's real mouse and keyboard stay free and the user can keep working; call sandbox_start first and launch apps with sandbox_launch. \"desktop\" drives the user's real pointer and keyboard through uinput; the user must not touch the computer while it runs.";
const DOUBLE_CLICK_GAP: Duration = Duration::from_millis(60);
const DRAG_STEP_PAUSE: Duration = Duration::from_millis(10);
const PASTE_SETTLE: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "lowercase")]
pub enum Target {
    #[default]
    Sandbox,
    Desktop,
}

impl Target {
    fn name(self) -> &'static str {
        match self {
            Self::Sandbox => "sandbox",
            Self::Desktop => "desktop",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScreenSize {
    width: u32,
    height: u32,
}

/// One primitive emitted through an `Injector`. Built by pure functions so the
/// sequences can be unit-tested without a compositor.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Move(u32, u32),
    Button(Button, bool),
    Key(u16, bool),
    /// Scroll notches: positive dy scrolls down, positive dx scrolls right.
    Scroll(i32, i32),
    /// XKB modifier mask to advertise on the sandbox keyboard.
    Modifiers(u32),
    /// Put text on the clipboard of the target, then the caller presses ctrl+v.
    Clipboard(String),
    Sleep(Duration),
}

struct OperationState {
    desktop: Box<dyn Injector>,
    sandbox: Option<Sandbox>,
    budget: Arc<Budget>,
    keymap: KeyMap,
    screens: HashMap<Target, ScreenSize>,
    calibration: Option<Calibration>,
    stop_release_done: bool,
}

impl OperationState {
    fn stop(&mut self) {
        self.budget.stop();
        if let Err(error) = self.release_on_stop() {
            tracing::error!(error = %error, "failed to release pressed inputs after SIGINT");
        }
    }

    fn release_everything(&mut self) -> Result<(), String> {
        let mut first_error = None;
        if let Err(error) = self.desktop.release_all() {
            first_error.get_or_insert(format!("desktop: {error:#}"));
        }
        if let Some(sandbox) = self.sandbox.as_mut()
            && let Err(error) = sandbox.release_all()
        {
            first_error.get_or_insert(format!("sandbox: {error:#}"));
        }
        first_error.map_or(Ok(()), Err)
    }

    fn release_on_stop(&mut self) -> Result<(), String> {
        if self.stop_release_done {
            return Ok(());
        }
        self.stop_release_done = true;
        self.release_everything()
            .map_err(|error| format!("release pressed inputs after stop failed: {error}"))
    }

    fn stopped_error(&mut self) -> String {
        let error = BudgetError::Stopped.to_string();
        match self.release_on_stop() {
            Ok(()) => error,
            Err(cleanup_error) => format!("{error}; {cleanup_error}"),
        }
    }

    async fn approve_injection(&mut self) -> Result<(), String> {
        match self.budget.before_injection().await {
            Ok(()) => Ok(()),
            Err(BudgetError::Stopped) => Err(self.stopped_error()),
            Err(error) => Err(error.to_string()),
        }
    }

    fn ensure_running(&mut self) -> Result<(), String> {
        if self.budget.is_stopped() {
            Err(self.stopped_error())
        } else {
            Ok(())
        }
    }

    fn screen(&self, target: Target) -> Result<ScreenSize, String> {
        self.screens.get(&target).copied().ok_or_else(|| {
            format!(
                "no screenshot is available for target {}; call screenshot first",
                target.name()
            )
        })
    }

    fn sandbox_display(&self) -> Option<String> {
        self.sandbox.as_ref().map(|s| s.display().to_owned())
    }

    fn injector(&mut self, target: Target) -> Result<&mut dyn Injector, String> {
        match target {
            Target::Desktop => Ok(self.desktop.as_mut()),
            Target::Sandbox => self
                .sandbox
                .as_mut()
                .map(|s| s as &mut dyn Injector)
                .ok_or_else(|| "sandbox is not running; call sandbox_start first".to_owned()),
        }
    }

    /// Map screenshot pixels of `target` into injector coordinates.
    fn map_point(&self, target: Target, x: i64, y: i64) -> Result<(u32, u32), String> {
        let screen = self.screen(target)?;
        match target {
            Target::Desktop => {
                let transform = match &self.calibration {
                    Some(c) if c.width == screen.width && c.height == screen.height => c.transform,
                    _ => Transform::linear(screen.width, screen.height)
                        .map_err(|error| format!("invalid screenshot dimensions: {error}"))?,
                };
                let point = transform
                    .map_pixel(x, y, screen.width, screen.height)
                    .map_err(|error| error.to_string())?;
                Ok((u32::from(point.x), u32::from(point.y)))
            }
            Target::Sandbox => {
                if x < 0 || y < 0 || x >= i64::from(screen.width) || y >= i64::from(screen.height) {
                    return Err(format!(
                        "({x}, {y}) is outside the sandbox screenshot {}x{}",
                        screen.width, screen.height
                    ));
                }
                Ok((x as u32, y as u32))
            }
        }
    }

    async fn run_steps(&mut self, target: Target, steps: Vec<Step>) -> Result<(), String> {
        self.approve_injection().await?;
        let display = self.sandbox_display();
        for step in steps {
            self.ensure_running()?;
            let result = match step {
                Step::Sleep(duration) => {
                    tokio::time::sleep(duration).await;
                    Ok(())
                }
                Step::Modifiers(mask) => {
                    if let (Target::Sandbox, Some(sandbox)) = (target, self.sandbox.as_mut()) {
                        sandbox.set_modifier_state(mask);
                    }
                    Ok(())
                }
                Step::Clipboard(text) => {
                    let env = if target == Target::Sandbox {
                        display.clone()
                    } else {
                        None
                    };
                    copy_to_clipboard(&text, env.as_deref()).await
                }
                Step::Move(x, y) => self
                    .injector(target)?
                    .move_abs(x, y)
                    .map_err(|error| format!("move failed: {error:#}")),
                Step::Button(button, pressed) => self
                    .injector(target)?
                    .button(button, pressed)
                    .map_err(|error| format!("button failed: {error:#}")),
                Step::Key(code, pressed) => self
                    .injector(target)?
                    .key(code, pressed)
                    .map_err(|error| format!("key failed: {error:#}")),
                Step::Scroll(dx, dy) => {
                    let (horizontal, vertical) = match target {
                        Target::Desktop => (dx, -dy),
                        Target::Sandbox => (dx, dy),
                    };
                    self.injector(target)?
                        .scroll(horizontal, vertical)
                        .map_err(|error| format!("scroll failed: {error:#}"))
                }
            };
            if let Err(error) = result {
                let cleanup = self.injector(target).and_then(|injector| {
                    injector.release_all().map_err(|error| format!("{error:#}"))
                });
                return Err(match cleanup {
                    Ok(()) => error,
                    Err(cleanup_error) => {
                        format!("{error}; releasing pressed inputs also failed: {cleanup_error}")
                    }
                });
            }
        }
        self.budget.injection_succeeded();
        Ok(())
    }
}

async fn copy_to_clipboard(text: &str, wayland_display: Option<&str>) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    // wl-copy forks a background process that keeps serving the clipboard, so
    // stdout/stderr must not be pipes: the fork would inherit them and the wait
    // for EOF would never end.
    let mut command = tokio::process::Command::new("wl-copy");
    if let Some(display) = wayland_display {
        command.env("WAYLAND_DISPLAY", display);
    }
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut child = command
        .spawn()
        .map_err(|error| format!("start wl-copy (is wl-clipboard installed?): {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .await
            .map_err(|error| format!("write to wl-copy: {error}"))?;
        drop(stdin);
    }
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .map_err(|_| "wl-copy timed out after 5 seconds".to_owned())?
        .map_err(|error| format!("wait for wl-copy: {error}"))?;
    if !status.success() {
        return Err(format!("wl-copy exited with {status}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pure step builders

fn click_steps(x: u32, y: u32, button: Button, count: u32) -> Vec<Step> {
    let mut steps = vec![Step::Move(x, y)];
    for index in 0..count {
        if index > 0 {
            steps.push(Step::Sleep(DOUBLE_CLICK_GAP));
        }
        steps.push(Step::Button(button, true));
        steps.push(Step::Button(button, false));
    }
    steps
}

fn drag_steps(points: &[(u32, u32)], step_px: u32, hold: Duration) -> Vec<Step> {
    let mut steps = Vec::new();
    let Some(&(start_x, start_y)) = points.first() else {
        return steps;
    };
    steps.push(Step::Move(start_x, start_y));
    steps.push(Step::Sleep(hold));
    steps.push(Step::Button(Button::Left, true));
    steps.push(Step::Sleep(hold));
    for pair in points.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        let dx = f64::from(to.0) - f64::from(from.0);
        let dy = f64::from(to.1) - f64::from(from.1);
        let distance = (dx * dx + dy * dy).sqrt();
        let count = (distance / f64::from(step_px.max(1))).ceil().max(1.0) as u32;
        for index in 1..=count {
            let t = f64::from(index) / f64::from(count);
            let x = (f64::from(from.0) + dx * t).round() as u32;
            let y = (f64::from(from.1) + dy * t).round() as u32;
            steps.push(Step::Move(x, y));
            steps.push(Step::Sleep(DRAG_STEP_PAUSE));
        }
    }
    steps.push(Step::Sleep(hold));
    steps.push(Step::Button(Button::Left, false));
    steps
}

fn combo_steps(keymap: &KeyMap, combo: &Combo) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    let mask = keymap.modifier_mask(&combo.modifiers);
    if mask != 0 {
        steps.push(Step::Modifiers(mask));
    }
    let mut modifier_codes = Vec::new();
    for modifier in &combo.modifiers {
        let code = keymap
            .modifier_evdev(*modifier)
            .map_err(|error| error.to_string())?;
        modifier_codes.push(code);
        steps.push(Step::Key(code, true));
    }
    steps.push(Step::Key(combo.key, true));
    steps.push(Step::Key(combo.key, false));
    for code in modifier_codes.iter().rev() {
        steps.push(Step::Key(*code, false));
    }
    if mask != 0 {
        steps.push(Step::Modifiers(0));
    }
    Ok(steps)
}

struct TypedPlan {
    steps: Vec<Step>,
    typed_chars: usize,
    pasted_chars: usize,
}

fn type_steps(keymap: &KeyMap, text: &str, delay: Duration) -> Result<TypedPlan, String> {
    let shift_code = keymap
        .modifier_evdev(Modifier::Shift)
        .map_err(|error| error.to_string())?;
    let shift_mask = keymap.modifier_mask(&[Modifier::Shift]);
    let paste_combo = keymap
        .parse_combo("ctrl+v")
        .map_err(|error| error.to_string())?;
    let mut plan = TypedPlan {
        steps: Vec::new(),
        typed_chars: 0,
        pasted_chars: 0,
    };
    for run in keymap.split_text(text) {
        match run {
            TextRun::Typed(strokes) => {
                for KeyStroke { evdev, shift } in strokes {
                    if shift {
                        plan.steps.push(Step::Modifiers(shift_mask));
                        plan.steps.push(Step::Key(shift_code, true));
                    }
                    plan.steps.push(Step::Key(evdev, true));
                    plan.steps.push(Step::Key(evdev, false));
                    if shift {
                        plan.steps.push(Step::Key(shift_code, false));
                        plan.steps.push(Step::Modifiers(0));
                    }
                    if !delay.is_zero() {
                        plan.steps.push(Step::Sleep(delay));
                    }
                    plan.typed_chars += 1;
                }
            }
            TextRun::Pasted(chunk) => {
                plan.pasted_chars += chunk.chars().count();
                plan.steps.push(Step::Clipboard(chunk));
                plan.steps.extend(combo_steps(keymap, &paste_combo)?);
                plan.steps.push(Step::Sleep(PASTE_SETTLE));
            }
        }
    }
    Ok(plan)
}

// ---------------------------------------------------------------------------
// Tool arguments

fn target_default() -> Option<Target> {
    None
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ObservationArgs {
    #[serde(default = "target_default")]
    #[schemars(description = "Which screen to capture: \"sandbox\" (default) or \"desktop\".")]
    pub target: Option<Target>,
    #[serde(default)]
    #[schemars(
        description = "Milliseconds to wait after the capture. Defaults to 150; valid range is 0 through 10000.",
        range(min = 0, max = 10000)
    )]
    pub settle_ms: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct PointArgs {
    #[schemars(
        description = "Horizontal pixel coordinate in the most recent screenshot of the target, origin top-left."
    )]
    pub x: i64,
    #[schemars(
        description = "Vertical pixel coordinate in the most recent screenshot of the target, origin top-left."
    )]
    pub y: i64,
    #[serde(default = "target_default")]
    #[schemars(description = "\"sandbox\" (default, recommended) or \"desktop\".")]
    pub target: Option<Target>,
    #[serde(default)]
    #[schemars(
        description = "Milliseconds to wait after the action so the UI can settle. Defaults to 150; valid range is 0 through 10000.",
        range(min = 0, max = 10000)
    )]
    pub settle_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct Point {
    pub x: i64,
    pub y: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct DragArgs {
    #[schemars(description = "Start point in screenshot pixels.")]
    pub from: Point,
    #[schemars(description = "End point in screenshot pixels.")]
    pub to: Point,
    #[serde(default)]
    #[schemars(description = "Optional intermediate points the pointer passes through, in order.")]
    pub waypoints: Vec<Point>,
    #[serde(default)]
    #[schemars(
        description = "Pixels per intermediate movement. Default 20.",
        range(min = 1, max = 500)
    )]
    pub step_px: Option<u32>,
    #[serde(default)]
    #[schemars(
        description = "Milliseconds to hold before pressing and before releasing. Default 50.",
        range(min = 0, max = 2000)
    )]
    pub hold_ms: Option<u64>,
    #[serde(default = "target_default")]
    #[schemars(description = "\"sandbox\" (default, recommended) or \"desktop\".")]
    pub target: Option<Target>,
    #[serde(default)]
    #[schemars(
        description = "Milliseconds to wait after releasing. Defaults to 150.",
        range(min = 0, max = 10000)
    )]
    pub settle_ms: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ScrollArgs {
    #[schemars(description = "Horizontal pixel coordinate to scroll at, in screenshot pixels.")]
    pub x: i64,
    #[schemars(description = "Vertical pixel coordinate to scroll at, in screenshot pixels.")]
    pub y: i64,
    #[serde(default)]
    #[schemars(description = "Wheel notches to the right (negative = left). Default 0.", range(min = -50, max = 50))]
    pub dx: Option<i32>,
    #[serde(default)]
    #[schemars(description = "Wheel notches down (negative = up). Default 0.", range(min = -50, max = 50))]
    pub dy: Option<i32>,
    #[serde(default = "target_default")]
    #[schemars(description = "\"sandbox\" (default, recommended) or \"desktop\".")]
    pub target: Option<Target>,
    #[serde(default)]
    #[schemars(
        description = "Milliseconds to wait after scrolling. Defaults to 150.",
        range(min = 0, max = 10000)
    )]
    pub settle_ms: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct KeyArgs {
    #[schemars(
        description = "Key combo such as \"ctrl+shift+t\", \"alt+F4\", \"Return\", \"Escape\". Modifiers: ctrl, shift, alt, super. The last token is an XKB keysym name."
    )]
    pub combo: String,
    #[serde(default = "target_default")]
    #[schemars(description = "\"sandbox\" (default, recommended) or \"desktop\".")]
    pub target: Option<Target>,
    #[serde(default)]
    #[schemars(
        description = "Milliseconds to wait after the key. Defaults to 150.",
        range(min = 0, max = 10000)
    )]
    pub settle_ms: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct TypeArgs {
    #[schemars(
        description = "Text to type into the focused window. Characters on the US layout are typed key by key; anything else (for example Chinese) is placed on the clipboard and pasted with ctrl+v."
    )]
    pub text: String,
    #[serde(default)]
    #[schemars(
        description = "Milliseconds between typed characters. Default 10.",
        range(min = 0, max = 1000)
    )]
    pub delay_ms: Option<u64>,
    #[serde(default = "target_default")]
    #[schemars(description = "\"sandbox\" (default, recommended) or \"desktop\".")]
    pub target: Option<Target>,
    #[serde(default)]
    #[schemars(
        description = "Milliseconds to wait after typing. Defaults to 150.",
        range(min = 0, max = 10000)
    )]
    pub settle_ms: Option<i64>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct NoArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct LaunchArgs {
    #[schemars(
        description = "Program and arguments, e.g. [\"gnome-text-editor\", \"--standalone\"]. Executed directly, not through a shell."
    )]
    pub argv: Vec<String>,
    #[serde(default)]
    #[schemars(description = "Working directory for the program.")]
    pub cwd: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScreenInfoOutput {
    target: &'static str,
    width: u32,
    height: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    scale_factor: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scale_factor_note: Option<&'static str>,
    coordinate_system: &'static str,
}

// ---------------------------------------------------------------------------
// Server

#[derive(Clone)]
pub struct DesktopServer {
    operation: Arc<tokio::sync::Mutex<OperationState>>,
    tool_router: ToolRouter<Self>,
}

fn error_result(text: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(text.into())])
}

fn text_result(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text.into())])
}

impl DesktopServer {
    pub fn new(desktop: Box<dyn Injector>, budget: Arc<Budget>, keymap: KeyMap) -> Self {
        Self {
            operation: Arc::new(tokio::sync::Mutex::new(OperationState {
                desktop,
                sandbox: None,
                budget,
                keymap,
                screens: HashMap::new(),
                calibration: calibrate::load_calibration(),
                stop_release_done: false,
            })),
            tool_router: Self::tool_router(),
        }
    }

    pub(crate) async fn stop(&self) {
        self.operation.lock().await.stop();
    }

    pub(crate) async fn shutdown(&self) {
        let mut operation = self.operation.lock().await;
        let _ = operation.release_everything();
        operation.sandbox = None;
    }

    async fn capture(
        &self,
        operation: &mut OperationState,
        target: Target,
    ) -> Result<CapturedScreenshot, String> {
        let captured = match target {
            Target::Desktop => screenshot::capture()
                .await
                .map_err(|error| format!("screenshot failed: {error:#}"))?,
            Target::Sandbox => {
                let sandbox = operation
                    .sandbox
                    .as_ref()
                    .ok_or_else(|| "sandbox is not running; call sandbox_start first".to_owned())?;
                let frame = tokio::task::block_in_place(|| sandbox.client().screenshot())
                    .map_err(|error| format!("sandbox screenshot failed: {error:#}"))?;
                CapturedScreenshot {
                    bytes: frame.png,
                    width: frame.width,
                    height: frame.height,
                    captured_at: chrono::Utc::now()
                        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                }
            }
        };
        operation.screens.insert(
            target,
            ScreenSize {
                width: captured.width,
                height: captured.height,
            },
        );
        operation.budget.screenshot_completed();
        Ok(captured)
    }

    async fn pointer_action(
        &self,
        args: PointArgs,
        button: Button,
        count: u32,
        verb: &str,
    ) -> Result<CallToolResult, String> {
        let target = args.target.unwrap_or_default();
        let settle = settle_duration(args.settle_ms)?;
        let mut operation = self.operation.lock().await;
        let (x, y) = operation.map_point(target, args.x, args.y)?;
        operation
            .run_steps(target, click_steps(x, y, button, count))
            .await?;
        tokio::time::sleep(settle).await;
        Ok(text_result(format!(
            "{verb} at {} pixel ({}, {})",
            target.name(),
            args.x,
            args.y
        )))
    }

    async fn move_action(&self, args: PointArgs) -> Result<CallToolResult, String> {
        let target = args.target.unwrap_or_default();
        let settle = settle_duration(args.settle_ms)?;
        let mut operation = self.operation.lock().await;
        let (x, y) = operation.map_point(target, args.x, args.y)?;
        operation.run_steps(target, vec![Step::Move(x, y)]).await?;
        tokio::time::sleep(settle).await;
        Ok(text_result(format!(
            "moved pointer to {} pixel ({}, {})",
            target.name(),
            args.x,
            args.y
        )))
    }

    async fn drag_action(&self, args: DragArgs) -> Result<CallToolResult, String> {
        let target = args.target.unwrap_or_default();
        let settle = settle_duration(args.settle_ms)?;
        let step_px = args.step_px.unwrap_or(20).clamp(1, 500);
        let hold = Duration::from_millis(args.hold_ms.unwrap_or(50).min(2000));
        let mut operation = self.operation.lock().await;
        let mut points = Vec::with_capacity(args.waypoints.len() + 2);
        for point in std::iter::once(args.from)
            .chain(args.waypoints.iter().copied())
            .chain(std::iter::once(args.to))
        {
            points.push(operation.map_point(target, point.x, point.y)?);
        }
        let steps = drag_steps(&points, step_px, hold);
        operation.run_steps(target, steps).await?;
        tokio::time::sleep(settle).await;
        Ok(text_result(format!(
            "dragged on {} from ({}, {}) to ({}, {}) through {} waypoint(s)",
            target.name(),
            args.from.x,
            args.from.y,
            args.to.x,
            args.to.y,
            args.waypoints.len()
        )))
    }

    async fn scroll_action(&self, args: ScrollArgs) -> Result<CallToolResult, String> {
        let target = args.target.unwrap_or_default();
        let settle = settle_duration(args.settle_ms)?;
        let dx = args.dx.unwrap_or(0).clamp(-50, 50);
        let dy = args.dy.unwrap_or(0).clamp(-50, 50);
        if dx == 0 && dy == 0 {
            return Err("dx and dy are both 0; nothing to scroll".to_owned());
        }
        let mut operation = self.operation.lock().await;
        let (x, y) = operation.map_point(target, args.x, args.y)?;
        let steps = vec![
            Step::Move(x, y),
            Step::Sleep(Duration::from_millis(20)),
            Step::Scroll(dx, dy),
        ];
        operation.run_steps(target, steps).await?;
        tokio::time::sleep(settle).await;
        Ok(text_result(format!(
            "scrolled dx={dx} dy={dy} notches at {} pixel ({}, {})",
            target.name(),
            args.x,
            args.y
        )))
    }

    async fn key_action(&self, args: KeyArgs) -> Result<CallToolResult, String> {
        let target = args.target.unwrap_or_default();
        let settle = settle_duration(args.settle_ms)?;
        let mut operation = self.operation.lock().await;
        let combo = operation
            .keymap
            .parse_combo(&args.combo)
            .map_err(|error| error.to_string())?;
        let steps = combo_steps(&operation.keymap, &combo)?;
        operation.run_steps(target, steps).await?;
        tokio::time::sleep(settle).await;
        Ok(text_result(format!(
            "pressed {} on {}",
            args.combo,
            target.name()
        )))
    }

    async fn type_action(&self, args: TypeArgs) -> Result<CallToolResult, String> {
        let target = args.target.unwrap_or_default();
        let settle = settle_duration(args.settle_ms)?;
        let delay = Duration::from_millis(args.delay_ms.unwrap_or(10).min(1000));
        if args.text.is_empty() {
            return Err("text is empty".to_owned());
        }
        let mut operation = self.operation.lock().await;
        let plan = type_steps(&operation.keymap, &args.text, delay)?;
        let (typed, pasted) = (plan.typed_chars, plan.pasted_chars);
        operation.run_steps(target, plan.steps).await?;
        tokio::time::sleep(settle).await;
        let mut message = format!("typed {typed} character(s) on {}", target.name());
        if pasted > 0 {
            message.push_str(&format!(
                "; {pasted} character(s) not on the US layout were pasted through the clipboard with ctrl+v"
            ));
        }
        Ok(text_result(message))
    }

    async fn calibrate_action(&self) -> Result<CallToolResult, String> {
        let mut operation = self.operation.lock().await;
        let mut shots: Vec<(u32, u32, calibrate::Image)> = Vec::new();
        for fraction in calibrate::PROBE_FRACTIONS {
            let (ax, ay) = calibrate::probe_abs(fraction);
            operation
                .run_steps(
                    Target::Desktop,
                    vec![Step::Move(ax, ay), Step::Sleep(Duration::from_millis(200))],
                )
                .await?;
            let captured = self.capture(&mut operation, Target::Desktop).await?;
            let image = calibrate::Image::from_png(&captured.bytes)
                .map_err(|error| format!("decode calibration screenshot: {error:#}"))?;
            shots.push((ax, ay, image));
        }
        let screen = operation.screen(Target::Desktop)?;
        let expected = |ax: u32, ay: u32| {
            (
                f64::from(ax) * f64::from(screen.width.saturating_sub(1))
                    / f64::from(crate::coords::ABSOLUTE_MAX),
                f64::from(ay) * f64::from(screen.height.saturating_sub(1))
                    / f64::from(crate::coords::ABSOLUTE_MAX),
            )
        };
        let (base_ax, base_ay, baseline) = &shots[0];
        let mut pairs: Vec<calibrate::Observation> = Vec::new();
        let mut baseline_seen = false;
        for (ax, ay, image) in &shots[1..] {
            let blobs =
                calibrate::changed_blobs(baseline, image).map_err(|error| error.to_string())?;
            if blobs.is_empty() {
                return Err(format!(
                    "calibration failed: no cursor-sized change between screenshots at ABS ({base_ax},{base_ay}) and ({ax},{ay}). Either the portal screenshot does not include the cursor or uinput moves are not reaching the compositor (run scripts/check.sh). Keeping the linear mapping."
                ));
            }
            let Some(blob) = calibrate::closest_blob(&blobs, expected(*ax, *ay)) else {
                continue;
            };
            pairs.push((blob.hotspot(), (f64::from(*ax), f64::from(*ay))));
            if !baseline_seen
                && blobs.len() >= 2
                && let Some(base_blob) =
                    calibrate::closest_blob(&blobs, expected(*base_ax, *base_ay))
                && base_blob != blob
            {
                pairs.push((
                    base_blob.hotspot(),
                    (f64::from(*base_ax), f64::from(*base_ay)),
                ));
                baseline_seen = true;
            }
        }
        let (transform, residual) =
            calibrate::fit_transform(&pairs).map_err(|error| error.to_string())?;
        let calibration = Calibration {
            width: screen.width,
            height: screen.height,
            transform,
            max_residual_px: residual,
            calibrated_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        };
        let path = calibrate::save_calibration(&calibration).map_err(|error| error.to_string())?;
        operation.calibration = Some(calibration);
        let verdict = if residual < 3.0 {
            "within the 3 px acceptance limit"
        } else {
            "ABOVE the 3 px acceptance limit; re-run with the desktop idle"
        };
        Ok(text_result(format!(
            "calibrated desktop target from {} cursor observations on a {}x{} screenshot; worst residual {residual:.2} px ({verdict}); saved to {}",
            pairs.len(),
            screen.width,
            screen.height,
            path.display()
        )))
    }

    async fn observe(
        &self,
        args: ObservationArgs,
        with_info: bool,
    ) -> Result<CallToolResult, String> {
        let target = args.target.unwrap_or_default();
        let settle = settle_duration(args.settle_ms)?;
        let mut operation = self.operation.lock().await;
        let scale_factor = if with_info && target == Target::Desktop {
            Some(read_scale_factor().await?)
        } else {
            None
        };
        let captured = self.capture(&mut operation, target).await?;
        drop(operation);
        tokio::time::sleep(settle).await;

        if with_info {
            let (scale_factor, scale_factor_note) = match scale_factor {
                Some(0) => (None, Some(AUTOMATIC_SCALE_FACTOR_NOTE)),
                other => (other, None),
            };
            let output = ScreenInfoOutput {
                target: target.name(),
                width: captured.width,
                height: captured.height,
                scale_factor,
                scale_factor_note,
                coordinate_system: COORDINATE_SYSTEM,
            };
            let text = serde_json::to_string(&output).map_err(|error| error.to_string())?;
            return Ok(text_result(text));
        }
        let image = ContentBlock::image(STANDARD.encode(&captured.bytes), "image/png");
        let metadata = ContentBlock::text(format!(
            "target={} timestamp={} width={} height={} (screenshot pixels)",
            target.name(),
            captured.captured_at,
            captured.width,
            captured.height
        ));
        Ok(CallToolResult::success(vec![image, metadata]))
    }
}

fn wrap(result: Result<CallToolResult, String>) -> Result<CallToolResult, McpError> {
    Ok(result.unwrap_or_else(error_result))
}

#[tool_router]
impl DesktopServer {
    #[tool(
        name = "screen_info",
        description = "Capture the target screen and report its width and height in screenshot pixels plus the coordinate system. Every input tool uses x,y pixel coordinates of the most recent screenshot of the same target, origin top-left. target: \"sandbox\" (default, recommended: does not touch the user's real mouse and keyboard) or \"desktop\" (the real screen)."
    )]
    async fn screen_info(
        &self,
        Parameters(args): Parameters<ObservationArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.observe(args, true).await)
    }

    #[tool(
        name = "screenshot",
        description = "Capture the target screen and return a PNG image plus timestamp and size in pixels. Coordinates for later actions are pixel positions in this image, origin top-left. target: \"sandbox\" (default, recommended: the nested sandbox window, user keeps working) or \"desktop\" (the real screen via the XDG portal)."
    )]
    async fn screenshot(
        &self,
        Parameters(args): Parameters<ObservationArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.observe(args, false).await)
    }

    #[tool(
        name = "move",
        description = "Move the pointer to x,y in screenshot pixels of the target (origin top-left). target: \"sandbox\" (default, recommended; does not move the user's real cursor) or \"desktop\" (moves the real cursor; the user must not touch the computer)."
    )]
    async fn move_pointer_tool(
        &self,
        Parameters(args): Parameters<PointArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.move_action(args).await)
    }

    #[tool(
        name = "click",
        description = "Move to x,y (screenshot pixels, origin top-left) and click the left button. target: \"sandbox\" (default, recommended; the user's real mouse stays free) or \"desktop\" (real mouse; the user must not touch the computer)."
    )]
    async fn click(
        &self,
        Parameters(args): Parameters<PointArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.pointer_action(args, Button::Left, 1, "clicked").await)
    }

    #[tool(
        name = "double_click",
        description = "Move to x,y (screenshot pixels, origin top-left) and double-click the left button, 60 ms apart. target: \"sandbox\" (default, recommended) or \"desktop\" (real mouse)."
    )]
    async fn double_click(
        &self,
        Parameters(args): Parameters<PointArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(
            self.pointer_action(args, Button::Left, 2, "double-clicked")
                .await,
        )
    }

    #[tool(
        name = "right_click",
        description = "Move to x,y (screenshot pixels, origin top-left) and click the right button. target: \"sandbox\" (default, recommended) or \"desktop\" (real mouse)."
    )]
    async fn right_click(
        &self,
        Parameters(args): Parameters<PointArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(
            self.pointer_action(args, Button::Right, 1, "right-clicked")
                .await,
        )
    }

    #[tool(
        name = "drag",
        description = "Press the left button at `from`, move through optional `waypoints` to `to`, then release. All points are screenshot pixels of the target, origin top-left. Use it for text selection and drag-and-drop. target: \"sandbox\" (default, recommended) or \"desktop\" (real mouse)."
    )]
    async fn drag(
        &self,
        Parameters(args): Parameters<DragArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.drag_action(args).await)
    }

    #[tool(
        name = "scroll",
        description = "Move to x,y (screenshot pixels) and scroll the wheel: dy notches down (negative = up), dx notches right (negative = left). target: \"sandbox\" (default, recommended) or \"desktop\" (real mouse)."
    )]
    async fn scroll(
        &self,
        Parameters(args): Parameters<ScrollArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.scroll_action(args).await)
    }

    #[tool(
        name = "key",
        description = "Press a key combination on the focused window, e.g. \"ctrl+shift+t\", \"alt+F4\", \"Return\", \"Escape\", \"ctrl+c\". Modifiers: ctrl, shift, alt, super; the last token is an XKB keysym name. Pure key injection, never a shell. target: \"sandbox\" (default, recommended) or \"desktop\" (real keyboard)."
    )]
    async fn key(&self, Parameters(args): Parameters<KeyArgs>) -> Result<CallToolResult, McpError> {
        wrap(self.key_action(args).await)
    }

    #[tool(
        name = "type",
        description = "Type text into the focused window. US-layout characters (ASCII letters, digits, punctuation, newline, tab) are typed key by key; any other character (for example Chinese) is put on the target's clipboard and pasted with ctrl+v, which requires the focused app to support paste. No input method is simulated. Pure key injection, never a shell. target: \"sandbox\" (default, recommended) or \"desktop\" (real keyboard)."
    )]
    async fn type_text(
        &self,
        Parameters(args): Parameters<TypeArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.type_action(args).await)
    }

    #[tool(
        name = "calibrate",
        description = "Desktop target only. Measures the mapping between screenshot pixels and the real pointer: moves the real cursor to 4 known positions, screenshots each, locates the cursor and fits an affine transform, then stores it for later desktop-target actions. The user must not touch the mouse while it runs (about 3 seconds). Not needed for the sandbox target, whose coordinates are exact by construction."
    )]
    async fn calibrate(
        &self,
        Parameters(_args): Parameters<NoArgs>,
    ) -> Result<CallToolResult, McpError> {
        wrap(self.calibrate_action().await)
    }

    #[tool(
        name = "sandbox_start",
        description = "Start the sandbox desktop: a nested sway compositor that appears as one window on the user's GNOME desktop. Apps launched with sandbox_launch run inside it and are driven without touching the user's real mouse or keyboard; the user can keep working and may move the window to another workspace. Returns the nested WAYLAND_DISPLAY name. Safe to call when already running."
    )]
    async fn sandbox_start(
        &self,
        Parameters(_args): Parameters<NoArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut operation = self.operation.lock().await;
        if let Some(display) = operation.sandbox_display() {
            return Ok(text_result(format!(
                "sandbox already running on WAYLAND_DISPLAY={display}"
            )));
        }
        let keymap = &operation.keymap;
        match tokio::task::block_in_place(|| Sandbox::start(keymap)) {
            Ok(sandbox) => {
                let display = sandbox.display().to_owned();
                operation.sandbox = Some(sandbox);
                operation.screens.remove(&Target::Sandbox);
                Ok(text_result(format!(
                    "sandbox started on WAYLAND_DISPLAY={display}; launch apps with sandbox_launch, then screenshot with target=sandbox"
                )))
            }
            Err(error) => Ok(error_result(format!("sandbox_start failed: {error:#}"))),
        }
    }

    #[tool(
        name = "sandbox_launch",
        description = "Launch a program inside the sandbox desktop (argv array, no shell). GTK apps that are already open on the real desktop may need a flag to force a new instance, e.g. [\"gnome-text-editor\", \"--standalone\"]. Returns the pid. Requires sandbox_start."
    )]
    async fn sandbox_launch(
        &self,
        Parameters(args): Parameters<LaunchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut operation = self.operation.lock().await;
        let Some(sandbox) = operation.sandbox.as_mut() else {
            return Ok(error_result(
                "sandbox is not running; call sandbox_start first",
            ));
        };
        match sandbox.launch(&args.argv, args.cwd.as_deref()) {
            Ok(pid) => Ok(text_result(format!(
                "launched {:?} inside the sandbox with pid {pid}",
                args.argv
            ))),
            Err(error) => Ok(error_result(format!("sandbox_launch failed: {error:#}"))),
        }
    }

    #[tool(
        name = "sandbox_stop",
        description = "Stop the sandbox desktop and every app launched inside it."
    )]
    async fn sandbox_stop(
        &self,
        Parameters(_args): Parameters<NoArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut operation = self.operation.lock().await;
        if operation.sandbox.is_none() {
            return Ok(text_result("sandbox was not running"));
        }
        let _ = operation
            .injector(Target::Sandbox)
            .and_then(|injector| injector.release_all().map_err(|error| error.to_string()));
        tokio::task::block_in_place(|| operation.sandbox = None);
        operation.screens.remove(&Target::Sandbox);
        Ok(text_result("sandbox stopped"))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DesktopServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("wayhand-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "wayhand-mcp drives a GNOME Wayland desktop: screenshot, then click/type/drag by screenshot pixel coordinates, then screenshot again to verify. {TARGET_DOC} Typical flow: sandbox_start -> sandbox_launch -> screenshot -> actions -> screenshot."
            ))
    }
}

async fn read_scale_factor() -> Result<u32, String> {
    let output = match tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("gsettings")
            .args(["get", "org.gnome.desktop.interface", "scaling-factor"])
            .output(),
    )
    .await
    {
        Ok(result) => {
            result.map_err(|error| format!("read GNOME scale factor with gsettings: {error}"))?
        }
        Err(_) => return Err("gsettings scale-factor query timed out after 5 seconds".to_owned()),
    };
    if !output.status.success() {
        let details = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(format!(
            "gsettings could not read the desktop scale factor{}",
            if details.is_empty() {
                String::new()
            } else {
                format!(": {details}")
            }
        ));
    }
    parse_scale_factor(&output.stdout)
}

fn parse_scale_factor(stdout: &[u8]) -> Result<u32, String> {
    let value = String::from_utf8(stdout.to_vec())
        .map_err(|error| format!("gsettings returned non-UTF-8 scale factor: {error}"))?;
    let raw = value
        .split_whitespace()
        .last()
        .ok_or_else(|| "gsettings returned an empty scale factor".to_owned())?;
    raw.parse::<u32>()
        .map_err(|error| format!("parse GNOME scale factor {raw:?}: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc, time::Duration};

    use super::{
        OperationState, ScreenSize, Step, Target, click_steps, combo_steps, drag_steps,
        parse_scale_factor, type_steps,
    };
    use crate::{
        inject::{Button, fake::FakeInjector},
        keys::KeyMap,
        safety::Budget,
    };

    fn state() -> OperationState {
        OperationState {
            desktop: Box::new(FakeInjector::new()),
            sandbox: None,
            budget: Arc::new(Budget::with_config(100, Duration::ZERO)),
            keymap: KeyMap::us().unwrap(),
            screens: HashMap::new(),
            calibration: None,
            stop_release_done: false,
        }
    }

    #[test]
    fn click_sequence_is_move_press_release() {
        assert_eq!(
            click_steps(5, 6, Button::Left, 1),
            vec![
                Step::Move(5, 6),
                Step::Button(Button::Left, true),
                Step::Button(Button::Left, false)
            ]
        );
        let double = click_steps(1, 1, Button::Left, 2);
        assert_eq!(double.len(), 6);
        assert!(matches!(double[3], Step::Sleep(_)));
    }

    #[test]
    fn drag_interpolates_and_releases_last() {
        let steps = drag_steps(&[(0, 0), (100, 0)], 25, Duration::ZERO);
        assert_eq!(steps.first(), Some(&Step::Move(0, 0)));
        assert_eq!(steps.last(), Some(&Step::Button(Button::Left, false)));
        let moves: Vec<_> = steps
            .iter()
            .filter_map(|s| match s {
                Step::Move(x, _) => Some(*x),
                _ => None,
            })
            .collect();
        assert_eq!(moves, vec![0, 25, 50, 75, 100]);
        assert_eq!(
            steps
                .iter()
                .filter(|s| matches!(s, Step::Button(_, true)))
                .count(),
            1
        );
    }

    #[test]
    fn drag_with_waypoint_visits_it() {
        let steps = drag_steps(&[(0, 0), (10, 0), (10, 10)], 10, Duration::ZERO);
        assert!(steps.contains(&Step::Move(10, 0)));
        assert_eq!(
            steps.iter().rev().find_map(|s| match s {
                Step::Move(x, y) => Some((*x, *y)),
                _ => None,
            }),
            Some((10, 10))
        );
    }

    #[test]
    fn combo_presses_modifiers_first_and_releases_in_reverse() {
        let keymap = KeyMap::us().unwrap();
        let combo = keymap.parse_combo("ctrl+shift+t").unwrap();
        let steps = combo_steps(&keymap, &combo).unwrap();
        let keys: Vec<_> = steps
            .iter()
            .filter_map(|s| match s {
                Step::Key(code, pressed) => Some((*code, *pressed)),
                _ => None,
            })
            .collect();
        assert_eq!(keys.len(), 6);
        assert_eq!(keys[0], (29, true));
        assert_eq!(keys[1], (42, true));
        assert_eq!(keys[2], (20, true));
        assert_eq!(keys[3], (20, false));
        assert_eq!(keys[4], (42, false));
        assert_eq!(keys[5], (29, false));
        assert!(matches!(steps.first(), Some(Step::Modifiers(m)) if *m != 0));
        assert_eq!(steps.last(), Some(&Step::Modifiers(0)));
    }

    #[test]
    fn type_plan_types_ascii_and_pastes_the_rest() {
        let keymap = KeyMap::us().unwrap();
        let plan = type_steps(&keymap, "Hi 中文!", Duration::ZERO).unwrap();
        assert_eq!(plan.typed_chars, 4);
        assert_eq!(plan.pasted_chars, 2);
        assert!(plan.steps.contains(&Step::Clipboard("中文".to_owned())));
        // 'H' needs shift: shift press precedes the key press
        let first_keys: Vec<_> = plan
            .steps
            .iter()
            .filter_map(|s| match s {
                Step::Key(code, pressed) => Some((*code, *pressed)),
                _ => None,
            })
            .take(2)
            .collect();
        assert_eq!(first_keys[0], (42, true));
        assert_eq!(first_keys[1], (35, true));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn desktop_steps_reach_fake_injector_and_scroll_sign_is_flipped() {
        let mut state = state();
        state.screens.insert(
            Target::Desktop,
            ScreenSize {
                width: 100,
                height: 100,
            },
        );
        let (x, y) = state.map_point(Target::Desktop, 99, 0).unwrap();
        assert_eq!((x, y), (65535, 0));
        state
            .run_steps(Target::Desktop, vec![Step::Move(x, y), Step::Scroll(0, 3)])
            .await
            .unwrap();
        assert_eq!(state.budget.consecutive_injections(), 1);
        assert!(state.map_point(Target::Desktop, 100, 0).is_err());
        assert!(
            state.map_point(Target::Sandbox, 1, 1).is_err(),
            "no sandbox screenshot yet"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn failing_injection_releases_pressed_inputs() {
        let mut fake = FakeInjector::new();
        fake.fail_after(1);
        let mut state = state();
        state.desktop = Box::new(fake);
        let error = state
            .run_steps(
                Target::Desktop,
                vec![Step::Button(Button::Left, true), Step::Move(1, 1)],
            )
            .await
            .unwrap_err();
        assert!(error.contains("move failed"), "{error}");
        assert_eq!(state.budget.consecutive_injections(), 0);
    }

    #[test]
    fn parses_gsettings_scale_factor() {
        assert_eq!(parse_scale_factor(b"uint32 2\n").unwrap(), 2);
        assert_eq!(parse_scale_factor(b"uint32 0\n").unwrap(), 0);
    }
}
