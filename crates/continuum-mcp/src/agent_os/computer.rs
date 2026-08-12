use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use continuum_core::config::AgentOsConfig;
use serde_json::Value;

use super::types::{
    AccessibilityRequest, ClickElementRequest, ClickRequest, FindElementRequest,
    FocusWindowRequest, KeyRequest, MouseButton, ObserveRequest, OpenUrlRequest, ScreenshotRequest,
    ScreenshotTarget, ScrollRequest, TypeRequest, WaitForElementRequest, WaitRequest,
};

pub struct ComputerBackend {
    screenshots_dir: PathBuf,
    temp_dir: PathBuf,
    ux: AgentOsConfig,
}

impl ComputerBackend {
    pub fn new(root: &Path, ux: AgentOsConfig) -> Result<Self> {
        let screenshots_dir = root.join("screenshots");
        let temp_dir = root.join("tmp");
        std::fs::create_dir_all(&screenshots_dir).with_context(|| {
            format!(
                "Failed to create computer-use screenshot directory {}",
                screenshots_dir.display()
            )
        })?;
        std::fs::create_dir_all(&temp_dir).with_context(|| {
            format!(
                "Failed to create computer-use temporary directory {}",
                temp_dir.display()
            )
        })?;
        Ok(Self {
            screenshots_dir,
            temp_dir,
            ux,
        })
    }

    pub fn status(&self) -> Value {
        serde_json::json!({
            "platform": std::env::consts::OS,
            "supported": cfg!(windows),
            "capabilities": {
                "window_observation": cfg!(windows),
                "accessibility_tree": cfg!(windows),
                "screenshots": cfg!(windows),
                "mouse_input": cfg!(windows),
                "keyboard_input": cfg!(windows),
                "semantic_element_targeting": cfg!(windows),
                "state_verification": true,
                "virtual_screen_click_guard": cfg!(windows),
                "verified_window_focus": cfg!(windows)
            },
            "action_cursor": {
                "enabled": self.ux.show_action_cursor,
                "duration_ms": self.ux.action_cursor_duration_ms.clamp(80, 3_000),
                "size_px": self.ux.action_cursor_size_px.clamp(18, 72)
            },
            "screenshots_dir": self.screenshots_dir,
            "implementation": "Windows UI Automation + Win32 input through isolated PowerShell child processes"
        })
    }

    pub async fn quick_state(&self) -> Result<Value> {
        self.observe(&ObserveRequest {
            include_windows: false,
            include_accessibility: false,
            include_screenshot: false,
            accessibility_max_nodes: 0,
            accessibility_max_depth: 0,
        })
        .await
    }

    /// A bounded post-action snapshot used by the Agent OS verifier. It keeps
    /// screenshots opt-in while still comparing the foreground window and a
    /// shallow UI Automation tree before and after a mutation.
    pub async fn verification_state(&self) -> Result<Value> {
        self.observe(&ObserveRequest {
            include_windows: false,
            include_accessibility: true,
            include_screenshot: false,
            accessibility_max_nodes: 300,
            accessibility_max_depth: 8,
        })
        .await
    }

    pub async fn observe(&self, request: &ObserveRequest) -> Result<Value> {
        ensure_windows()?;
        let mut env = BTreeMap::new();
        env.insert(
            "CONTINUUM_INCLUDE_WINDOWS".to_string(),
            request.include_windows.to_string(),
        );
        let mut observation = run_powershell_json(OBSERVE_PS, env, Duration::from_secs(12)).await?;

        if request.include_accessibility {
            let tree = self
                .accessibility(&AccessibilityRequest {
                    window_handle: observation
                        .pointer("/foreground/handle")
                        .and_then(Value::as_i64),
                    max_nodes: request.accessibility_max_nodes,
                    max_depth: request.accessibility_max_depth,
                })
                .await?;
            observation["accessibility"] = tree;
        }
        if request.include_screenshot {
            let screenshot = self
                .screenshot(&ScreenshotRequest {
                    target: ScreenshotTarget::ForegroundWindow,
                })
                .await?;
            observation["screenshot"] = screenshot;
        }
        Ok(observation)
    }

    pub async fn list_windows(&self) -> Result<Value> {
        self.observe(&ObserveRequest {
            include_windows: true,
            include_accessibility: false,
            include_screenshot: false,
            accessibility_max_nodes: 0,
            accessibility_max_depth: 0,
        })
        .await
        .map(|value| {
            serde_json::json!({
                "foreground": value.get("foreground").cloned().unwrap_or(Value::Null),
                "windows": value.get("windows").cloned().unwrap_or_else(|| Value::Array(Vec::new())),
                "monitors": value.get("monitors").cloned().unwrap_or_else(|| Value::Array(Vec::new()))
            })
        })
    }

    pub async fn accessibility(&self, request: &AccessibilityRequest) -> Result<Value> {
        ensure_windows()?;
        let mut env = BTreeMap::new();
        env.insert(
            "CONTINUUM_WINDOW_HANDLE".to_string(),
            request.window_handle.unwrap_or(0).to_string(),
        );
        env.insert(
            "CONTINUUM_MAX_NODES".to_string(),
            request.max_nodes.clamp(1, 1500).to_string(),
        );
        env.insert(
            "CONTINUUM_MAX_DEPTH".to_string(),
            request.max_depth.clamp(1, 30).to_string(),
        );
        run_powershell_json(ACCESSIBILITY_PS, env, Duration::from_secs(20)).await
    }

    pub async fn screenshot(&self, request: &ScreenshotRequest) -> Result<Value> {
        ensure_windows()?;
        let target = match request.target {
            ScreenshotTarget::ForegroundWindow => "foreground_window",
            ScreenshotTarget::VirtualScreen => "virtual_screen",
        };
        let filename = format!(
            "{}-{}-{}.png",
            chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ"),
            target,
            uuid::Uuid::new_v4().simple()
        );
        let path = self.screenshots_dir.join(filename);
        let mut env = BTreeMap::new();
        env.insert(
            "CONTINUUM_SCREENSHOT_PATH".to_string(),
            path.to_string_lossy().into_owned(),
        );
        env.insert(
            "CONTINUUM_SCREENSHOT_TARGET".to_string(),
            target.to_string(),
        );
        run_powershell_json(SCREENSHOT_PS, env, Duration::from_secs(20)).await
    }

    pub async fn find_element(&self, request: &FindElementRequest) -> Result<Value> {
        validate_selector(request)?;
        let tree = self
            .accessibility(&AccessibilityRequest {
                window_handle: request.window_handle,
                max_nodes: request.max_nodes,
                max_depth: request.max_depth,
            })
            .await?;
        find_matching_node(&tree, request).ok_or_else(|| {
            anyhow::anyhow!(
                "No visible, enabled accessible element matched name={:?}, automation_id={:?}, control_type={:?}, class_name={:?}",
                request.name,
                request.automation_id,
                request.control_type,
                request.class_name
            )
        })
    }

    pub async fn click(&self, request: &ClickRequest) -> Result<Value> {
        ensure_windows()?;
        if !(1..=3).contains(&request.count) {
            bail!("click count must be between 1 and 3");
        }
        let mut env = BTreeMap::new();
        env.insert("CONTINUUM_X".to_string(), request.x.to_string());
        env.insert("CONTINUUM_Y".to_string(), request.y.to_string());
        env.insert(
            "CONTINUUM_MOUSE_BUTTON".to_string(),
            match request.button {
                MouseButton::Left => "left",
                MouseButton::Right => "right",
                MouseButton::Middle => "middle",
            }
            .to_string(),
        );
        env.insert(
            "CONTINUUM_CLICK_COUNT".to_string(),
            request.count.to_string(),
        );
        env.insert(
            "CONTINUUM_SHOW_AGENT_CURSOR".to_string(),
            self.ux.show_action_cursor.to_string(),
        );
        env.insert(
            "CONTINUUM_AGENT_CURSOR_MS".to_string(),
            self.ux
                .action_cursor_duration_ms
                .clamp(80, 3_000)
                .to_string(),
        );
        env.insert(
            "CONTINUUM_AGENT_CURSOR_SIZE".to_string(),
            self.ux.action_cursor_size_px.clamp(18, 72).to_string(),
        );
        let result = run_powershell_json(CLICK_PS, env, Duration::from_secs(10)).await?;
        delay(request.post_action_delay_ms).await;
        Ok(result)
    }

    pub async fn click_element(&self, request: &ClickElementRequest) -> Result<Value> {
        let element = self.find_element(&request.selector).await?;
        ensure_actionable_element(&element)?;
        let bounds = element
            .get("bounds")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow::anyhow!("Matched element has no usable bounds"))?;
        let x = bounds.get("x").and_then(Value::as_f64).unwrap_or_default();
        let y = bounds.get("y").and_then(Value::as_f64).unwrap_or_default();
        let width = bounds
            .get("width")
            .and_then(Value::as_f64)
            .unwrap_or_default();
        let height = bounds
            .get("height")
            .and_then(Value::as_f64)
            .unwrap_or_default();
        if width <= 0.0 || height <= 0.0 {
            bail!("Matched element has an empty bounding rectangle");
        }
        let click = self
            .click(&ClickRequest {
                x: (x + width / 2.0).round() as i32,
                y: (y + height / 2.0).round() as i32,
                button: request.button,
                count: request.count,
                post_action_delay_ms: request.post_action_delay_ms,
            })
            .await?;
        Ok(serde_json::json!({ "element": element, "click": click }))
    }

    pub async fn type_text(&self, request: &TypeRequest) -> Result<Value> {
        ensure_windows()?;
        if request.text.chars().count() > 100_000 {
            bail!("text is too long (maximum 100,000 characters per call)");
        }
        if request.replace_existing {
            self.key(&KeyRequest {
                keys: vec!["CTRL".to_string(), "A".to_string()],
                post_action_delay_ms: 80,
            })
            .await?;
        }
        if request.text.is_empty() {
            return Ok(serde_json::json!({ "typed_characters": 0, "clipboard_restored": true }));
        }
        let text_path = self
            .temp_dir
            .join(format!("type-{}.txt", uuid::Uuid::new_v4().simple()));
        let write_result = (|| -> Result<()> {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&text_path)
                .with_context(|| format!("Failed to create {}", text_path.display()))?;
            file.write_all(request.text.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })();
        write_result?;

        let mut env = BTreeMap::new();
        env.insert(
            "CONTINUUM_TEXT_PATH".to_string(),
            text_path.to_string_lossy().into_owned(),
        );
        let result = run_powershell_json(TYPE_PS, env, Duration::from_secs(30)).await;
        let _ = std::fs::remove_file(&text_path);
        let result = result?;
        delay(request.post_action_delay_ms).await;
        Ok(result)
    }

    pub async fn key(&self, request: &KeyRequest) -> Result<Value> {
        ensure_windows()?;
        if request.keys.is_empty() || request.keys.len() > 8 {
            bail!("keys must contain between 1 and 8 entries");
        }
        for key in &request.keys {
            if key.is_empty() || key.chars().count() > 32 {
                bail!("each key must contain between 1 and 32 characters");
            }
        }
        let mut env = BTreeMap::new();
        env.insert(
            "CONTINUUM_KEYS_JSON".to_string(),
            serde_json::to_string(&request.keys)?,
        );
        let result = run_powershell_json(KEY_PS, env, Duration::from_secs(10)).await?;
        delay(request.post_action_delay_ms).await;
        Ok(result)
    }

    pub async fn scroll(&self, request: &ScrollRequest) -> Result<Value> {
        ensure_windows()?;
        if request.amount == 0 || request.amount.unsigned_abs() > 100 {
            bail!("scroll amount must be between -100 and 100, excluding zero");
        }
        let mut env = BTreeMap::new();
        env.insert(
            "CONTINUUM_SCROLL_AMOUNT".to_string(),
            request.amount.to_string(),
        );
        env.insert(
            "CONTINUUM_SCROLL_HORIZONTAL".to_string(),
            request.horizontal.to_string(),
        );
        let result = run_powershell_json(SCROLL_PS, env, Duration::from_secs(10)).await?;
        delay(request.post_action_delay_ms).await;
        Ok(result)
    }

    pub async fn focus_window(&self, request: &FocusWindowRequest) -> Result<Value> {
        ensure_windows()?;
        if request.handle.is_none()
            && request
                .title_contains
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
            && request
                .process_name
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
        {
            bail!("provide handle, title_contains, or process_name");
        }
        let mut env = BTreeMap::new();
        env.insert(
            "CONTINUUM_WINDOW_HANDLE".to_string(),
            request.handle.unwrap_or(0).to_string(),
        );
        env.insert(
            "CONTINUUM_TITLE_CONTAINS".to_string(),
            request.title_contains.clone().unwrap_or_default(),
        );
        env.insert(
            "CONTINUUM_PROCESS_NAME".to_string(),
            request.process_name.clone().unwrap_or_default(),
        );
        let result = run_powershell_json(FOCUS_PS, env, Duration::from_secs(12)).await?;
        delay(request.post_action_delay_ms).await;
        Ok(result)
    }

    pub async fn open_url(&self, request: &OpenUrlRequest) -> Result<Value> {
        ensure_windows()?;
        let parsed = url::Url::parse(&request.url).context("URL is invalid")?;
        if !matches!(parsed.scheme(), "http" | "https") {
            bail!("only http and https URLs may be opened");
        }
        let mut env = BTreeMap::new();
        env.insert("CONTINUUM_URL".to_string(), parsed.to_string());
        let result = run_powershell_json(OPEN_URL_PS, env, Duration::from_secs(12)).await?;
        delay(request.post_action_delay_ms).await;
        Ok(result)
    }

    pub async fn wait(&self, request: &WaitRequest) -> Result<Value> {
        if request.milliseconds > 300_000 {
            bail!("wait is capped at 300,000 ms");
        }
        delay(request.milliseconds).await;
        Ok(serde_json::json!({ "waited_ms": request.milliseconds }))
    }

    pub async fn wait_for_element(&self, request: &WaitForElementRequest) -> Result<Value> {
        validate_selector(&request.selector)?;
        let timeout_ms = request.timeout_ms.clamp(100, 120_000);
        let poll_ms = request.poll_interval_ms.clamp(100, 5_000);
        let started = std::time::Instant::now();
        let mut attempts = 0_u64;
        loop {
            attempts += 1;
            match self.find_element(&request.selector).await {
                Ok(element) => {
                    return Ok(serde_json::json!({
                        "found": true,
                        "attempts": attempts,
                        "elapsed_ms": started.elapsed().as_millis(),
                        "element": element
                    }))
                }
                Err(error) if started.elapsed().as_millis() < u128::from(timeout_ms) => {
                    tracing::debug!(
                        layer = "agent_os",
                        component = "computer_use",
                        attempt = attempts,
                        error = %error,
                        "Element not present yet"
                    );
                    delay(poll_ms).await;
                }
                Err(error) => {
                    bail!(
                        "element did not appear within {timeout_ms} ms after {attempts} attempts: {error}"
                    )
                }
            }
        }
    }
}

fn validate_selector(request: &FindElementRequest) -> Result<()> {
    let fields = [
        request.name.as_deref(),
        request.automation_id.as_deref(),
        request.control_type.as_deref(),
        request.class_name.as_deref(),
    ];
    if fields
        .iter()
        .all(|value| value.map(str::trim).unwrap_or_default().is_empty())
    {
        bail!("selector needs at least one of name, automation_id, control_type, or class_name");
    }
    if fields
        .iter()
        .flatten()
        .any(|value| value.chars().count() > 512)
    {
        bail!("selector values are capped at 512 characters");
    }
    Ok(())
}

fn ensure_actionable_element(element: &Value) -> Result<()> {
    if !element
        .get("is_enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!("Matched element is disabled and will not be clicked");
    }
    if element
        .get("is_offscreen")
        .and_then(Value::as_bool)
        .unwrap_or(true)
    {
        bail!("Matched element is offscreen and will not be clicked");
    }
    Ok(())
}

fn find_matching_node(tree: &Value, request: &FindElementRequest) -> Option<Value> {
    let nodes = tree.get("nodes")?.as_array()?;
    let matches = |field: &str, expected: &Option<String>, node: &Value| -> bool {
        let Some(expected) = expected.as_deref().map(str::trim).filter(|v| !v.is_empty()) else {
            return true;
        };
        let actual = node.get(field).and_then(Value::as_str).unwrap_or_default();
        if request.exact {
            actual.eq_ignore_ascii_case(expected)
        } else {
            actual
                .to_ascii_lowercase()
                .contains(&expected.to_ascii_lowercase())
        }
    };
    nodes
        .iter()
        .filter(|node| matches("name", &request.name, node))
        .filter(|node| matches("automation_id", &request.automation_id, node))
        .filter(|node| matches("control_type", &request.control_type, node))
        .filter(|node| matches("class_name", &request.class_name, node))
        .filter(|node| {
            node.get("is_enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && !node
                    .get("is_offscreen")
                    .and_then(Value::as_bool)
                    .unwrap_or(true)
        })
        .max_by_key(|node| {
            node.get("is_keyboard_focusable")
                .and_then(Value::as_bool)
                .unwrap_or(false) as u8
        })
        .cloned()
}

async fn delay(milliseconds: u64) {
    if milliseconds > 0 {
        tokio::time::sleep(Duration::from_millis(milliseconds.min(30_000))).await;
    }
}

fn ensure_windows() -> Result<()> {
    if cfg!(windows) {
        Ok(())
    } else {
        bail!("computer use is currently implemented for the Windows Continuum desktop target")
    }
}

#[cfg(windows)]
async fn run_powershell_json(
    script: &'static str,
    env: BTreeMap<String, String>,
    timeout: Duration,
) -> Result<Value> {
    let mut command = tokio::process::Command::new("powershell.exe");
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-STA",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        script,
    ]);
    for (key, value) in env {
        command.env(key, value);
    }
    command.kill_on_drop(true);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .context("PowerShell computer-use action timed out")??;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("PowerShell computer-use action failed: {}", stderr.trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_json_output(&stdout)
}

#[cfg(not(windows))]
async fn run_powershell_json(
    _script: &'static str,
    _env: BTreeMap<String, String>,
    _timeout: Duration,
) -> Result<Value> {
    bail!("PowerShell computer-use backend is only available on Windows")
}

fn parse_json_output(stdout: &str) -> Result<Value> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        bail!("computer-use backend returned an empty response");
    }
    if let Ok(value) = serde_json::from_str(trimmed) {
        return Ok(value);
    }
    for line in trimmed.lines().rev() {
        if let Ok(value) = serde_json::from_str(line.trim()) {
            return Ok(value);
        }
    }
    bail!(
        "computer-use backend returned invalid JSON: {}",
        trimmed.chars().take(1200).collect::<String>()
    )
}

const OBSERVE_PS: &str = r#"
$ErrorActionPreference = 'Stop'
$null = Add-Type -AssemblyName System.Windows.Forms
$source = @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class ContinuumWindowApi {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr hWnd, StringBuilder text, int count);
  [DllImport("user32.dll")] public static extern int GetWindowTextLength(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint processId);
}
'@
$null = Add-Type -TypeDefinition $source -Language CSharp
function Window-Info([IntPtr]$handle) {
  if ($handle -eq [IntPtr]::Zero) { return $null }
  $length = [ContinuumWindowApi]::GetWindowTextLength($handle)
  $builder = New-Object System.Text.StringBuilder ([Math]::Max($length + 1, 2))
  $null = [ContinuumWindowApi]::GetWindowText($handle, $builder, $builder.Capacity)
  [uint32]$pid = 0
  $null = [ContinuumWindowApi]::GetWindowThreadProcessId($handle, [ref]$pid)
  $rect = New-Object ContinuumWindowApi+RECT
  $hasRect = [ContinuumWindowApi]::GetWindowRect($handle, [ref]$rect)
  $process = $null
  try { $process = Get-Process -Id $pid -ErrorAction Stop } catch {}
  [ordered]@{
    handle = $handle.ToInt64()
    title = $builder.ToString()
    process_id = [int64]$pid
    process_name = if ($process) { $process.ProcessName } else { $null }
    bounds = if ($hasRect) { [ordered]@{ x=$rect.Left; y=$rect.Top; width=($rect.Right-$rect.Left); height=($rect.Bottom-$rect.Top) } } else { $null }
  }
}
$foregroundHandle = [ContinuumWindowApi]::GetForegroundWindow()
$foreground = Window-Info $foregroundHandle
$cursor = [System.Windows.Forms.Cursor]::Position
$monitors = @([System.Windows.Forms.Screen]::AllScreens | ForEach-Object {
  [ordered]@{
    name = $_.DeviceName
    primary = $_.Primary
    bounds = [ordered]@{ x=$_.Bounds.X; y=$_.Bounds.Y; width=$_.Bounds.Width; height=$_.Bounds.Height }
    working_area = [ordered]@{ x=$_.WorkingArea.X; y=$_.WorkingArea.Y; width=$_.WorkingArea.Width; height=$_.WorkingArea.Height }
  }
})
$windows = @()
if ($env:CONTINUUM_INCLUDE_WINDOWS -eq 'true') {
  $windows = @(Get-Process | Where-Object { $_.MainWindowHandle -ne 0 } | ForEach-Object {
    try { Window-Info ([IntPtr]$_.MainWindowHandle) } catch {}
  } | Where-Object { $_ -ne $null } | Sort-Object process_name, title)
}
[ordered]@{
  platform = 'windows'
  captured_at = [DateTime]::UtcNow.ToString('o')
  foreground = $foreground
  cursor = [ordered]@{ x=$cursor.X; y=$cursor.Y }
  monitors = $monitors
  windows = $windows
} | ConvertTo-Json -Compress -Depth 8
"#;

const ACCESSIBILITY_PS: &str = r#"
$ErrorActionPreference = 'Stop'
$null = Add-Type -AssemblyName UIAutomationClient
$null = Add-Type -AssemblyName UIAutomationTypes
$source = @'
using System;
using System.Runtime.InteropServices;
public static class ContinuumForegroundApi {
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
}
'@
$null = Add-Type -TypeDefinition $source -Language CSharp
$handleValue = [int64]$env:CONTINUUM_WINDOW_HANDLE
$handle = if ($handleValue -ne 0) { [IntPtr]$handleValue } else { [ContinuumForegroundApi]::GetForegroundWindow() }
if ($handle -eq [IntPtr]::Zero) { throw 'No foreground window is available' }
$root = [System.Windows.Automation.AutomationElement]::FromHandle($handle)
if ($null -eq $root) { throw 'Windows UI Automation could not resolve the target window' }
$maxNodes = [Math]::Max(1, [Math]::Min(1500, [int]$env:CONTINUUM_MAX_NODES))
$maxDepth = [Math]::Max(1, [Math]::Min(30, [int]$env:CONTINUUM_MAX_DEPTH))
$walker = [System.Windows.Automation.TreeWalker]::ControlViewWalker
$script:nodes = New-Object System.Collections.Generic.List[object]
function Read-Node($element, [int]$depth, [string]$parentId) {
  if ($null -eq $element -or $script:nodes.Count -ge $maxNodes -or $depth -gt $maxDepth) { return }
  $index = $script:nodes.Count
  $id = "n_$index"
  try {
    $current = $element.Current
    $rect = $current.BoundingRectangle
    $controlType = if ($current.ControlType) { $current.ControlType.ProgrammaticName -replace '^ControlType\\.', '' } else { '' }
    $script:nodes.Add([ordered]@{
      id = $id
      parent_id = $parentId
      depth = $depth
      name = $current.Name
      automation_id = $current.AutomationId
      control_type = $controlType
      class_name = $current.ClassName
      framework_id = $current.FrameworkId
      help_text = $current.HelpText
      is_enabled = $current.IsEnabled
      is_offscreen = $current.IsOffscreen
      is_keyboard_focusable = $current.IsKeyboardFocusable
      has_keyboard_focus = $current.HasKeyboardFocus
      bounds = [ordered]@{ x=$rect.X; y=$rect.Y; width=$rect.Width; height=$rect.Height }
    })
  } catch { return }
  if ($depth -ge $maxDepth -or $script:nodes.Count -ge $maxNodes) { return }
  $child = $null
  try { $child = $walker.GetFirstChild($element) } catch {}
  while ($null -ne $child -and $script:nodes.Count -lt $maxNodes) {
    Read-Node $child ($depth + 1) $id
    try { $child = $walker.GetNextSibling($child) } catch { $child = $null }
  }
}
Read-Node $root 0 $null
[ordered]@{
  window_handle = $handle.ToInt64()
  node_count = $script:nodes.Count
  truncated = ($script:nodes.Count -ge $maxNodes)
  max_nodes = $maxNodes
  max_depth = $maxDepth
  nodes = @($script:nodes)
} | ConvertTo-Json -Compress -Depth 9
"#;

const SCREENSHOT_PS: &str = r#"
$ErrorActionPreference = 'Stop'
$null = Add-Type -AssemblyName System.Drawing
$null = Add-Type -AssemblyName System.Windows.Forms
$source = @'
using System;
using System.Runtime.InteropServices;
public static class ContinuumCaptureApi {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left; public int Top; public int Right; public int Bottom; }
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);
}
'@
$null = Add-Type -TypeDefinition $source -Language CSharp
$target = $env:CONTINUUM_SCREENSHOT_TARGET
if ($target -eq 'virtual_screen') {
  $rect = [System.Windows.Forms.SystemInformation]::VirtualScreen
  $x=$rect.X; $y=$rect.Y; $width=$rect.Width; $height=$rect.Height; $handle=0
} else {
  $hwnd = [ContinuumCaptureApi]::GetForegroundWindow()
  if ($hwnd -eq [IntPtr]::Zero) { throw 'No foreground window is available' }
  $nativeRect = New-Object ContinuumCaptureApi+RECT
  if (-not [ContinuumCaptureApi]::GetWindowRect($hwnd, [ref]$nativeRect)) { throw 'GetWindowRect failed' }
  $x=$nativeRect.Left; $y=$nativeRect.Top; $width=($nativeRect.Right-$nativeRect.Left); $height=($nativeRect.Bottom-$nativeRect.Top); $handle=$hwnd.ToInt64()
}
if ($width -le 0 -or $height -le 0) { throw 'Screenshot target has empty bounds' }
$bitmap = [System.Drawing.Bitmap]::new($width, $height, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
try {
  $size = [System.Drawing.Size]::new($width, $height)
  $graphics.CopyFromScreen($x, $y, 0, 0, $size, [System.Drawing.CopyPixelOperation]::SourceCopy)
  $bitmap.Save($env:CONTINUUM_SCREENSHOT_PATH, [System.Drawing.Imaging.ImageFormat]::Png)
} finally {
  $graphics.Dispose()
  $bitmap.Dispose()
}
[ordered]@{
  path = $env:CONTINUUM_SCREENSHOT_PATH
  target = $target
  window_handle = $handle
  bounds = [ordered]@{ x=$x; y=$y; width=$width; height=$height }
  captured_at = [DateTime]::UtcNow.ToString('o')
} | ConvertTo-Json -Compress -Depth 5
"#;

const FOCUS_PS: &str = r#"
$ErrorActionPreference = 'Stop'
$source = @'
using System;
using System.Runtime.InteropServices;
public static class ContinuumFocusApi {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool ShowWindowAsync(IntPtr hWnd, int nCmdShow);
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
}
'@
$null = Add-Type -TypeDefinition $source -Language CSharp
$handleValue = [int64]$env:CONTINUUM_WINDOW_HANDLE
$selected = $null
if ($handleValue -ne 0) {
  $selected = Get-Process | Where-Object { [int64]$_.MainWindowHandle -eq $handleValue } | Select-Object -First 1
} else {
  $title = $env:CONTINUUM_TITLE_CONTAINS
  $processName = $env:CONTINUUM_PROCESS_NAME
  $selected = Get-Process | Where-Object {
    $_.MainWindowHandle -ne 0 -and
    ([string]::IsNullOrWhiteSpace($title) -or $_.MainWindowTitle.IndexOf($title, [StringComparison]::OrdinalIgnoreCase) -ge 0) -and
    ([string]::IsNullOrWhiteSpace($processName) -or $_.ProcessName.Equals($processName, [StringComparison]::OrdinalIgnoreCase))
  } | Select-Object -First 1
}
if ($null -eq $selected) { throw 'No matching top-level window was found' }
$handle = [IntPtr]$selected.MainWindowHandle
$null = [ContinuumFocusApi]::ShowWindowAsync($handle, 9)
$null = [ContinuumFocusApi]::BringWindowToTop($handle)
$requested = [ContinuumFocusApi]::SetForegroundWindow($handle)
Start-Sleep -Milliseconds 120
$actual = [ContinuumFocusApi]::GetForegroundWindow()
$verified = ($actual -eq $handle)
if (-not $requested -or -not $verified) {
  throw "Windows did not confirm the requested foreground window (requested=$requested, actual=$($actual.ToInt64()), expected=$($handle.ToInt64()))"
}
[ordered]@{
  focused = $true
  verified = $verified
  handle = $handle.ToInt64()
  process_id = $selected.Id
  process_name = $selected.ProcessName
  title = $selected.MainWindowTitle
} | ConvertTo-Json -Compress -Depth 4
"#;

const CLICK_PS: &str = r#"
$ErrorActionPreference = 'Stop'
$null = Add-Type -AssemblyName System.Windows.Forms
$source = @'
using System;
using System.Runtime.InteropServices;
public static class ContinuumMouseApi {
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extraInfo);
}
'@
$null = Add-Type -TypeDefinition $source -Language CSharp
$x = [int]$env:CONTINUUM_X
$y = [int]$env:CONTINUUM_Y
$virtual = [System.Windows.Forms.SystemInformation]::VirtualScreen
if (-not $virtual.Contains($x, $y)) {
  throw "Click coordinate ($x, $y) is outside the virtual screen $($virtual.X),$($virtual.Y),$($virtual.Width),$($virtual.Height)"
}
$count = [Math]::Max(1, [Math]::Min(3, [int]$env:CONTINUUM_CLICK_COUNT))
$button = $env:CONTINUUM_MOUSE_BUTTON
switch ($button) {
  'right' { $down=[uint32]0x0008; $up=[uint32]0x0010 }
  'middle' { $down=[uint32]0x0020; $up=[uint32]0x0040 }
  default { $down=[uint32]0x0002; $up=[uint32]0x0004 }
}
$cursorShown = $false
$cursorError = $null
$marker = $null
if ($env:CONTINUUM_SHOW_AGENT_CURSOR -eq 'true') {
  try {
    $null = Add-Type -AssemblyName PresentationFramework
    $size = [Math]::Max(18, [Math]::Min(72, [int]$env:CONTINUUM_AGENT_CURSOR_SIZE))
    $duration = [Math]::Max(80, [Math]::Min(3000, [int]$env:CONTINUUM_AGENT_CURSOR_MS))
    $marker = New-Object System.Windows.Window
    $marker.WindowStyle = [System.Windows.WindowStyle]::None
    $marker.ResizeMode = [System.Windows.ResizeMode]::NoResize
    $marker.AllowsTransparency = $true
    $marker.Background = [System.Windows.Media.Brushes]::Transparent
    $marker.Topmost = $true
    $marker.ShowInTaskbar = $false
    $marker.ShowActivated = $false
    $marker.Width = $size
    $marker.Height = $size
    # Offset keeps the AI marker distinct from the user's physical pointer.
    $marker.Left = $x + 12
    $marker.Top = $y + 12
    $badge = New-Object System.Windows.Controls.Border
    $badge.CornerRadius = New-Object System.Windows.CornerRadius ($size / 2)
    $badge.Background = New-Object System.Windows.Media.SolidColorBrush ([System.Windows.Media.Color]::FromArgb(225, 245, 158, 11))
    $badge.BorderBrush = [System.Windows.Media.Brushes]::White
    $badge.BorderThickness = New-Object System.Windows.Thickness 2
    $label = New-Object System.Windows.Controls.TextBlock
    $label.Text = 'AI'
    $label.Foreground = [System.Windows.Media.Brushes]::Black
    $label.FontWeight = [System.Windows.FontWeights]::Bold
    $label.FontSize = [Math]::Max(9, [Math]::Round($size * 0.34))
    $label.HorizontalAlignment = [System.Windows.HorizontalAlignment]::Center
    $label.VerticalAlignment = [System.Windows.VerticalAlignment]::Center
    $badge.Child = $label
    $marker.Content = $badge
    $marker.Show()
    # Paint before input so the AI pointer is visible during the action.
    $marker.Dispatcher.Invoke(
      [System.Action]{},
      [System.Windows.Threading.DispatcherPriority]::Render
    )
    $cursorShown = $true
  } catch {
    $cursorError = $_.Exception.Message
    try { if ($null -ne $marker) { $marker.Close() } } catch {}
  }
}
if (-not [ContinuumMouseApi]::SetCursorPos($x, $y)) { throw 'SetCursorPos failed' }
for ($i=0; $i -lt $count; $i++) {
  [ContinuumMouseApi]::mouse_event($down, 0, 0, 0, [UIntPtr]::Zero)
  [ContinuumMouseApi]::mouse_event($up, 0, 0, 0, [UIntPtr]::Zero)
  if ($i + 1 -lt $count) { Start-Sleep -Milliseconds 90 }
}
if ($cursorShown) {
  Start-Sleep -Milliseconds $duration
  try { $marker.Close() } catch {
    if ($null -eq $cursorError) { $cursorError = $_.Exception.Message }
  }
}
[ordered]@{
  x=$x
  y=$y
  button=$button
  count=$count
  agent_cursor_shown=$cursorShown
  agent_cursor_error=$cursorError
  virtual_screen=[ordered]@{ x=$virtual.X; y=$virtual.Y; width=$virtual.Width; height=$virtual.Height }
} | ConvertTo-Json -Compress -Depth 3
"#;

const TYPE_PS: &str = r#"
$ErrorActionPreference = 'Stop'
$null = Add-Type -AssemblyName System.Windows.Forms
$textPath = $env:CONTINUUM_TEXT_PATH
if ([string]::IsNullOrWhiteSpace($textPath) -or -not (Test-Path -LiteralPath $textPath -PathType Leaf)) {
  throw 'Typing payload file is unavailable'
}
$text = [System.IO.File]::ReadAllText($textPath, [System.Text.Encoding]::UTF8)
$previous = $null
$hadPrevious = $false
$restored = $false
try {
  $previous = [System.Windows.Forms.Clipboard]::GetDataObject()
  $hadPrevious = ($null -ne $previous)
} catch {}
try {
  [System.Windows.Forms.Clipboard]::SetText($text)
  [System.Windows.Forms.SendKeys]::SendWait('^v')
  Start-Sleep -Milliseconds 75
} finally {
  try {
    if ($hadPrevious) { [System.Windows.Forms.Clipboard]::SetDataObject($previous, $true) }
    else { [System.Windows.Forms.Clipboard]::Clear() }
    $restored = $true
  } catch {}
}
[ordered]@{ typed_characters=$text.Length; clipboard_restored=$restored } | ConvertTo-Json -Compress
"#;

const KEY_PS: &str = r#"
$ErrorActionPreference = 'Stop'
$null = Add-Type -AssemblyName System.Windows.Forms
$keys = @($env:CONTINUUM_KEYS_JSON | ConvertFrom-Json)
$modifier = ''
$body = ''
foreach ($raw in $keys) {
  $key = ([string]$raw).ToUpperInvariant()
  switch ($key) {
    'CTRL' { $modifier += '^'; continue }
    'CONTROL' { $modifier += '^'; continue }
    'SHIFT' { $modifier += '+'; continue }
    'ALT' { $modifier += '%'; continue }
    'ENTER' { $body += '{ENTER}'; continue }
    'RETURN' { $body += '{ENTER}'; continue }
    'TAB' { $body += '{TAB}'; continue }
    'ESC' { $body += '{ESC}'; continue }
    'ESCAPE' { $body += '{ESC}'; continue }
    'BACKSPACE' { $body += '{BACKSPACE}'; continue }
    'DELETE' { $body += '{DELETE}'; continue }
    'DEL' { $body += '{DELETE}'; continue }
    'UP' { $body += '{UP}'; continue }
    'DOWN' { $body += '{DOWN}'; continue }
    'LEFT' { $body += '{LEFT}'; continue }
    'RIGHT' { $body += '{RIGHT}'; continue }
    'HOME' { $body += '{HOME}'; continue }
    'END' { $body += '{END}'; continue }
    'PAGEUP' { $body += '{PGUP}'; continue }
    'PAGEDOWN' { $body += '{PGDN}'; continue }
    'SPACE' { $body += ' '; continue }
  }
  if ($key -match '^F([1-9]|1[0-2])$') { $body += "{$key}"; continue }
  if ($key -eq 'WIN' -or $key -eq 'WINDOWS') { throw 'WIN-key shortcuts are not supported by this backend' }
  $escaped = ([string]$raw) -replace '([+^%~(){}\\[\\]])', '{$1}'
  $body += $escaped
}
if ([string]::IsNullOrEmpty($body)) { throw 'No non-modifier key was supplied' }
$sequence = $modifier + $body
[System.Windows.Forms.SendKeys]::SendWait($sequence)
[ordered]@{ keys=$keys; send_keys_sequence=$sequence } | ConvertTo-Json -Compress -Depth 3
"#;

const SCROLL_PS: &str = r#"
$ErrorActionPreference = 'Stop'
$source = @'
using System;
using System.Runtime.InteropServices;
public static class ContinuumScrollApi {
  [DllImport("user32.dll")] private static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extraInfo);
  public static void Scroll(int delta, bool horizontal) {
    uint flags = horizontal ? 0x01000u : 0x0800u;
    mouse_event(flags, 0, 0, unchecked((uint)delta), UIntPtr.Zero);
  }
}
'@
$null = Add-Type -TypeDefinition $source -Language CSharp
$amount = [int]$env:CONTINUUM_SCROLL_AMOUNT
$horizontal = ($env:CONTINUUM_SCROLL_HORIZONTAL -eq 'true')
$delta = $amount * 120
[ContinuumScrollApi]::Scroll($delta, $horizontal)
[ordered]@{ amount=$amount; horizontal=$horizontal; wheel_delta=$delta } | ConvertTo-Json -Compress
"#;

const OPEN_URL_PS: &str = r#"
$ErrorActionPreference = 'Stop'
$url = $env:CONTINUUM_URL
$process = Start-Process -FilePath $url -PassThru
[ordered]@{ opened=$true; url=$url; process_id=if ($process) { $process.Id } else { $null } } | ConvertTo-Json -Compress
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn selector() -> FindElementRequest {
        FindElementRequest {
            window_handle: None,
            name: Some("save".into()),
            automation_id: None,
            control_type: Some("button".into()),
            class_name: None,
            exact: false,
            max_nodes: 20,
            max_depth: 4,
        }
    }

    #[test]
    fn semantic_selector_prefers_visible_enabled_node() {
        let tree = serde_json::json!({
            "nodes": [
                {"name":"Save", "automation_id":"", "control_type":"Button", "class_name":"", "is_enabled":false, "is_offscreen":false},
                {"name":"Save", "automation_id":"save", "control_type":"Button", "class_name":"", "is_enabled":true, "is_offscreen":false}
            ]
        });
        let node = find_matching_node(&tree, &selector()).expect("match");
        assert_eq!(node["automation_id"], "save");
    }

    #[test]
    fn semantic_selector_refuses_disabled_or_offscreen_only_matches() {
        for node in [
            serde_json::json!({"name":"Save", "control_type":"Button", "is_enabled":false, "is_offscreen":false}),
            serde_json::json!({"name":"Save", "control_type":"Button", "is_enabled":true, "is_offscreen":true}),
        ] {
            let tree = serde_json::json!({"nodes":[node]});
            assert!(find_matching_node(&tree, &selector()).is_none());
        }
    }

    #[test]
    fn click_guard_requires_actionable_metadata() {
        assert!(ensure_actionable_element(&serde_json::json!({
            "is_enabled": true,
            "is_offscreen": false
        }))
        .is_ok());
        assert!(ensure_actionable_element(&serde_json::json!({
            "is_enabled": false,
            "is_offscreen": false
        }))
        .is_err());
        assert!(ensure_actionable_element(&serde_json::json!({
            "is_enabled": true,
            "is_offscreen": true
        }))
        .is_err());
    }

    #[test]
    fn empty_selector_is_rejected() {
        let request = FindElementRequest {
            window_handle: None,
            name: None,
            automation_id: None,
            control_type: None,
            class_name: None,
            exact: false,
            max_nodes: 20,
            max_depth: 4,
        };
        assert!(validate_selector(&request).is_err());
    }

    #[test]
    fn click_script_reports_visual_agent_cursor_without_masking_click_result() {
        assert!(CLICK_PS.contains("CONTINUUM_SHOW_AGENT_CURSOR"));
        assert!(CLICK_PS.contains("$cursorError = $_.Exception.Message"));
        assert!(CLICK_PS.contains("agent_cursor_shown=$cursorShown"));
        assert!(CLICK_PS.contains("agent_cursor_error=$cursorError"));
        assert!(
            CLICK_PS.find("$marker.Show()").expect("marker")
                < CLICK_PS.find("mouse_event($down").expect("click")
        );
        assert!(CLICK_PS.contains("DispatcherPriority]::Render"));
    }
}
