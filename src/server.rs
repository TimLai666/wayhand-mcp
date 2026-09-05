use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

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
    coords::{AbsolutePoint, Transform},
    inject::{Button, Injector},
    safety::{Budget, BudgetError, settle_duration},
    screenshot::{self, CapturedScreenshot},
};

pub const COORDINATE_SYSTEM: &str =
    "x,y are pixel coordinates in the most recent screenshot, origin top-left";
const AUTOMATIC_SCALE_FACTOR_NOTE: &str = "GNOME scaling-factor is 0 (automatic); coordinates are screenshot pixels and do not depend on this value";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScreenSize {
    width: u32,
    height: u32,
}

struct OperationState {
    injector: Box<dyn Injector>,
    budget: Arc<Budget>,
    stop_release_done: bool,
}

impl OperationState {
    fn new(injector: Box<dyn Injector>, budget: Arc<Budget>) -> Self {
        Self {
            injector,
            budget,
            stop_release_done: false,
        }
    }

    fn stop(&mut self) {
        self.budget.stop();
        if let Err(error) = self.release_on_stop() {
            tracing::error!(error = %error, "failed to release pressed inputs after SIGINT");
        }
    }

    fn release_on_stop(&mut self) -> Result<(), String> {
        if self.stop_release_done {
            return Ok(());
        }
        self.stop_release_done = true;
        self.injector
            .release_all()
            .map_err(|error| format!("release pressed inputs after stop failed: {error:#}"))
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

    fn ensure_running_before_emit(&mut self) -> Result<(), String> {
        if self.budget.is_stopped() {
            Err(self.stopped_error())
        } else {
            Ok(())
        }
    }

    fn injection_failed(&mut self, error: String) -> String {
        match self.injector.release_all() {
            Ok(()) => error,
            Err(cleanup_error) => {
                format!("{error}; releasing pressed inputs also failed: {cleanup_error:#}")
            }
        }
    }
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ObservationArgs {
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
        description = "Horizontal coordinate in screenshot pixels. Coordinates use the most recent screenshot, with origin at the top-left."
    )]
    pub x: i64,
    #[schemars(
        description = "Vertical coordinate in screenshot pixels. Coordinates use the most recent screenshot, with origin at the top-left."
    )]
    pub y: i64,
    #[serde(default)]
    #[schemars(
        description = "Milliseconds to wait after the input action. Defaults to 150; valid range is 0 through 10000.",
        range(min = 0, max = 10000)
    )]
    pub settle_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScreenInfoOutput {
    width: u32,
    height: u32,
    scale_factor: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scale_factor_note: Option<&'static str>,
    coordinate_system: &'static str,
}

#[derive(Clone)]
pub struct DesktopServer {
    operation: Arc<tokio::sync::Mutex<OperationState>>,
    screen: Arc<Mutex<Option<ScreenSize>>>,
    tool_router: ToolRouter<Self>,
}

impl DesktopServer {
    pub fn new(injector: Box<dyn Injector>, budget: Arc<Budget>) -> Self {
        Self {
            operation: Arc::new(tokio::sync::Mutex::new(OperationState::new(
                injector, budget,
            ))),
            screen: Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    pub(crate) async fn stop(&self) {
        let mut operation = self.operation.lock().await;
        operation.stop();
    }

    async fn capture(&self) -> Result<CapturedScreenshot, String> {
        screenshot::capture()
            .await
            .map_err(|error| format!("screenshot failed: {error:#}"))
    }

    fn remember_capture(&self, captured: &CapturedScreenshot, budget: &Budget) {
        let size = ScreenSize {
            width: captured.width,
            height: captured.height,
        };
        let mut screen = self
            .screen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *screen = Some(size);
        drop(screen);
        budget.screenshot_completed();
    }

    fn latest_screen(&self) -> Result<ScreenSize, String> {
        self.screen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .copied()
            .ok_or_else(|| {
                "no screenshot is available; call screenshot before move or click".to_owned()
            })
    }

    async fn move_pointer(&self, args: PointArgs) -> Result<CallToolResult, String> {
        let mut operation = self.operation.lock().await;
        let screen = self.latest_screen()?;
        let (point, settle) = validate_point(&args, screen)?;
        operation.approve_injection().await?;
        operation.ensure_running_before_emit()?;
        if let Err(error) = operation.injector.move_abs(point.x, point.y) {
            return Err(operation.injection_failed(format!("move injection failed: {error:#}")));
        }
        operation.budget.injection_succeeded();
        tokio::time::sleep(settle).await;

        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "moved pointer to screenshot pixel ({}, {})",
            args.x, args.y
        ))]))
    }

    async fn click_pointer(&self, args: PointArgs) -> Result<CallToolResult, String> {
        let mut operation = self.operation.lock().await;
        let screen = self.latest_screen()?;
        let (point, settle) = validate_point(&args, screen)?;
        operation.approve_injection().await?;
        operation.ensure_running_before_emit()?;
        if let Err(error) = operation.injector.move_abs(point.x, point.y) {
            return Err(operation.injection_failed(format!("click injection failed: {error:#}")));
        }
        operation.ensure_running_before_emit()?;
        if let Err(error) = operation.injector.button(Button::Left, true) {
            return Err(operation.injection_failed(format!("click injection failed: {error:#}")));
        }
        operation.ensure_running_before_emit()?;
        if let Err(error) = operation.injector.button(Button::Left, false) {
            return Err(operation.injection_failed(format!("click injection failed: {error:#}")));
        }
        operation.budget.injection_succeeded();
        tokio::time::sleep(settle).await;

        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
            "clicked at screenshot pixel ({}, {})",
            args.x, args.y
        ))]))
    }
}

#[tool_router]
impl DesktopServer {
    #[tool(
        name = "screen_info",
        description = "Capture the current screen and report width and height in screenshot pixels, the desktop scale factor, and the coordinate system. Every input tool uses x,y pixel coordinates in the most recent screenshot, origin top-left. settle_ms is an integer number of milliseconds to wait after the capture."
    )]
    async fn screen_info(
        &self,
        Parameters(args): Parameters<ObservationArgs>,
    ) -> Result<CallToolResult, McpError> {
        let operation = self.operation.lock().await;
        let settle = match settle_duration(args.settle_ms) {
            Ok(settle) => settle,
            Err(error) => return Ok(CallToolResult::error(vec![ContentBlock::text(error)])),
        };
        let scale_factor = match read_scale_factor().await {
            Ok(scale_factor) => scale_factor,
            Err(error) => return Ok(CallToolResult::error(vec![ContentBlock::text(error)])),
        };
        let captured = match self.capture().await {
            Ok(captured) => captured,
            Err(error) => return Ok(CallToolResult::error(vec![ContentBlock::text(error)])),
        };
        self.remember_capture(&captured, &operation.budget);
        tokio::time::sleep(settle).await;

        let (scale_factor, scale_factor_note) = if scale_factor == 0 {
            (None, Some(AUTOMATIC_SCALE_FACTOR_NOTE))
        } else {
            (Some(scale_factor), None)
        };
        let output = ScreenInfoOutput {
            width: captured.width,
            height: captured.height,
            scale_factor,
            scale_factor_note,
            coordinate_system: COORDINATE_SYSTEM,
        };
        let text = serde_json::to_string(&output)
            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        name = "screenshot",
        description = "Capture the current screen through the XDG desktop portal and return a PNG image plus its ISO-8601 timestamp and dimensions in screenshot pixels. The coordinate system is x,y pixel coordinates in the most recent screenshot, origin top-left. settle_ms is an integer number of milliseconds to wait after the capture."
    )]
    async fn screenshot(
        &self,
        Parameters(args): Parameters<ObservationArgs>,
    ) -> Result<CallToolResult, McpError> {
        let operation = self.operation.lock().await;
        let settle = match settle_duration(args.settle_ms) {
            Ok(settle) => settle,
            Err(error) => return Ok(CallToolResult::error(vec![ContentBlock::text(error)])),
        };
        let captured = match self.capture().await {
            Ok(captured) => captured,
            Err(error) => return Ok(CallToolResult::error(vec![ContentBlock::text(error)])),
        };
        self.remember_capture(&captured, &operation.budget);
        tokio::time::sleep(settle).await;

        let image = ContentBlock::image(STANDARD.encode(&captured.bytes), "image/png");
        let metadata = ContentBlock::text(format!(
            "timestamp={} width={} height={} (screenshot pixels)",
            captured.captured_at, captured.width, captured.height
        ));
        Ok(CallToolResult::success(vec![image, metadata]))
    }

    #[tool(
        name = "move",
        description = "Move the real pointer to x,y. x and y are integer pixel coordinates in the most recent screenshot, origin top-left. Coordinates are screenshot pixels, not desktop points. settle_ms is an integer number of milliseconds to wait after the move."
    )]
    async fn move_pointer_tool(
        &self,
        Parameters(args): Parameters<PointArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.move_pointer(args).await {
            Ok(result) => Ok(result),
            Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(error)])),
        }
    }

    #[tool(
        name = "click",
        description = "Move the real pointer and click the left mouse button at x,y. x and y are integer pixel coordinates in the most recent screenshot, origin top-left. Coordinates are screenshot pixels, not desktop points. settle_ms is an integer number of milliseconds to wait after the click."
    )]
    async fn click(
        &self,
        Parameters(args): Parameters<PointArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.click_pointer(args).await {
            Ok(result) => Ok(result),
            Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(error)])),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DesktopServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "desktop-driver",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "This server controls the user's real Wayland mouse and keyboard through uinput.",
            )
    }
}

fn validate_point(
    args: &PointArgs,
    screen: ScreenSize,
) -> Result<(AbsolutePoint, Duration), String> {
    let settle = settle_duration(args.settle_ms)?;
    let transform = Transform::linear(screen.width, screen.height)
        .map_err(|error| format!("invalid screenshot dimensions: {error}"))?;
    let point = transform
        .map_pixel(args.x, args.y, screen.width, screen.height)
        .map_err(|error| error.to_string())?;
    Ok((point, settle))
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
    let factor = raw
        .parse::<u32>()
        .map_err(|error| format!("parse GNOME scale factor {raw:?}: {error}"))?;
    Ok(factor)
}

#[cfg(test)]
mod tests {
    use super::{
        AUTOMATIC_SCALE_FACTOR_NOTE, COORDINATE_SYSTEM, PointArgs, ScreenInfoOutput, ScreenSize,
        parse_scale_factor, validate_point,
    };

    fn args(x: i64, y: i64) -> PointArgs {
        PointArgs {
            x,
            y,
            settle_ms: Some(0),
        }
    }

    #[test]
    fn point_validation_rejects_negative_coordinates() {
        let screen = ScreenSize {
            width: 1920,
            height: 1080,
        };
        assert!(validate_point(&args(-1, 0), screen).is_err());
        assert!(validate_point(&args(0, -1), screen).is_err());
    }

    #[test]
    fn point_validation_rejects_coordinates_past_screenshot_edges() {
        let screen = ScreenSize {
            width: 1920,
            height: 1080,
        };
        assert!(validate_point(&args(1920, 0), screen).is_err());
        assert!(validate_point(&args(0, 1080), screen).is_err());
    }

    #[test]
    fn coordinate_system_text_is_explicit() {
        assert_eq!(
            COORDINATE_SYSTEM,
            "x,y are pixel coordinates in the most recent screenshot, origin top-left"
        );
    }

    #[test]
    fn parses_gsettings_scale_factor() {
        assert_eq!(parse_scale_factor(b"uint32 2\n").unwrap(), 2);
        assert_eq!(parse_scale_factor(b"uint32 0\n").unwrap(), 0);
    }

    #[test]
    fn automatic_scale_factor_is_reported_as_null_with_note() {
        let output = ScreenInfoOutput {
            width: 1920,
            height: 1080,
            scale_factor: None,
            scale_factor_note: Some(AUTOMATIC_SCALE_FACTOR_NOTE),
            coordinate_system: COORDINATE_SYSTEM,
        };
        let json = serde_json::to_value(output).unwrap();

        assert_eq!(json["scaleFactor"], serde_json::Value::Null);
        assert_eq!(json["scaleFactorNote"], AUTOMATIC_SCALE_FACTOR_NOTE);
    }

    #[test]
    fn nonzero_scale_factor_is_reported_as_number_without_note() {
        let output = ScreenInfoOutput {
            width: 1920,
            height: 1080,
            scale_factor: Some(2),
            scale_factor_note: None,
            coordinate_system: COORDINATE_SYSTEM,
        };
        let json = serde_json::to_value(output).unwrap();

        assert_eq!(json["scaleFactor"], 2);
        assert!(json.get("scaleFactorNote").is_none());
    }
}
