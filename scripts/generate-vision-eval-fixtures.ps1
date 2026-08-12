# Generates deterministic, synthetic desktop screenshots for the local vision
# quality benchmark. No captured user content is used.

[CmdletBinding()]
param(
    [string]$OutputDir = (Join-Path $PSScriptRoot "..\crates\continuum-vision\tests\fixtures\generated")
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null

function New-Canvas {
    param([System.Drawing.Color]$Background)
    $bitmap = [System.Drawing.Bitmap]::new(1280, 720)
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $graphics.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::ClearTypeGridFit
    $graphics.Clear($Background)
    return [PSCustomObject]@{ Bitmap = $bitmap; Graphics = $graphics }
}

function Save-Canvas {
    param($Canvas, [string]$Name)
    $path = Join-Path $OutputDir $Name
    $Canvas.Bitmap.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $Canvas.Graphics.Dispose()
    $Canvas.Bitmap.Dispose()
    Write-Host "[OK] $path"
}

function Font([float]$Size, [System.Drawing.FontStyle]$Style = [System.Drawing.FontStyle]::Regular) {
    return [System.Drawing.Font]::new("Segoe UI", $Size, $Style)
}

function Brush([string]$HtmlColor) {
    return [System.Drawing.SolidBrush]::new([System.Drawing.ColorTranslator]::FromHtml($HtmlColor))
}

# IDE with an unmistakable build failure dialog.
$canvas = New-Canvas ([System.Drawing.ColorTranslator]::FromHtml("#171A21"))
$g = $canvas.Graphics
$g.FillRectangle((Brush "#232834"), 0, 0, 1280, 58)
$g.DrawString("Continuum - Visual Studio Code", (Font 20 Bold), (Brush "#F4F7FB"), 24, 14)
$g.FillRectangle((Brush "#20242E"), 0, 58, 230, 662)
$g.DrawString("EXPLORER", (Font 14 Bold), (Brush "#B8C1D1"), 22, 82)
$g.DrawString("continuum-core`n  src`n    config.rs`n    main.rs`n  Cargo.toml", (Font 16), (Brush "#D8DEE9"), 28, 126)
$g.DrawString("fn load_config() {", (Font 22), (Brush "#C792EA"), 285, 120)
$g.DrawString('    read_file("config.toml")?;', (Font 22), (Brush "#C3E88D"), 285, 164)
$g.DrawString("}", (Font 22), (Brush "#C792EA"), 285, 208)
$g.FillRectangle((Brush "#F7F8FA"), 370, 265, 650, 260)
$g.FillRectangle((Brush "#C62828"), 370, 265, 650, 58)
$g.DrawString("BUILD FAILED", (Font 23 Bold), (Brush "#FFFFFF"), 398, 278)
$g.DrawString("File not found: config.toml", (Font 25 Bold), (Brush "#20242E"), 410, 356)
$g.DrawString("The application could not start.", (Font 19), (Brush "#434A57"), 410, 410)
$g.FillRectangle((Brush "#246BCE"), 795, 463, 170, 42)
$g.DrawString("Close", (Font 16 Bold), (Brush "#FFFFFF"), 852, 472)
Save-Canvas $canvas "ide-build-error.png"

# Operational dashboard: should not be classified as an error.
$canvas = New-Canvas ([System.Drawing.ColorTranslator]::FromHtml("#F4F7FB"))
$g = $canvas.Graphics
$g.FillRectangle((Brush "#111827"), 0, 0, 1280, 72)
$g.DrawString("Continuum Runtime Dashboard", (Font 24 Bold), (Brush "#FFFFFF"), 36, 19)
$g.FillRectangle((Brush "#DCFCE7"), 45, 105, 1190, 88)
$g.DrawString("ALL SYSTEMS OPERATIONAL", (Font 27 Bold), (Brush "#166534"), 82, 130)
$g.DrawString("Vision health", (Font 19 Bold), (Brush "#1F2937"), 62, 238)
$g.DrawString("Healthy", (Font 18 Bold), (Brush "#15803D"), 1060, 240)
$g.DrawString("Observations per minute", (Font 18), (Brush "#4B5563"), 62, 292)
$heights = @(115, 170, 145, 240, 205, 285, 250, 315)
for ($i = 0; $i -lt $heights.Count; $i++) {
    $x = 90 + ($i * 125)
    $g.FillRectangle((Brush "#3B82F6"), $x, 650 - $heights[$i], 72, $heights[$i])
}
$g.DrawString("No failures detected", (Font 18), (Brush "#166534"), 900, 665)
Save-Canvas $canvas "runtime-dashboard.png"

# Privacy settings with clear toggle state and no sensitive data.
$canvas = New-Canvas ([System.Drawing.ColorTranslator]::FromHtml("#EEF1F6"))
$g = $canvas.Graphics
$g.FillRectangle((Brush "#FFFFFF"), 180, 70, 920, 580)
$g.DrawString("Privacy settings", (Font 30 Bold), (Brush "#111827"), 230, 115)
$g.DrawString("Control what Continuum observes locally", (Font 18), (Brush "#6B7280"), 232, 172)
$rows = @(
    @("Screen observation", "ON", "#16A34A"),
    @("Save screenshots", "OFF", "#6B7280"),
    @("Microphone", "ON", "#16A34A")
)
for ($i = 0; $i -lt $rows.Count; $i++) {
    $y = 255 + ($i * 105)
    $g.DrawString($rows[$i][0], (Font 22 Bold), (Brush "#1F2937"), 250, $y)
    $g.FillRectangle((Brush $rows[$i][2]), 850, $y - 2, 150, 52)
    $g.DrawString($rows[$i][1], (Font 17 Bold), (Brush "#FFFFFF"), 895, $y + 8)
}
$g.DrawString("Processing stays on this device", (Font 18 Bold), (Brush "#2563EB"), 250, 578)
Save-Canvas $canvas "privacy-settings.png"

# Calendar-like planning screen.
$canvas = New-Canvas ([System.Drawing.ColorTranslator]::FromHtml("#FFFFFF"))
$g = $canvas.Graphics
$g.FillRectangle((Brush "#4338CA"), 0, 0, 1280, 82)
$g.DrawString("Team Calendar - April", (Font 25 Bold), (Brush "#FFFFFF"), 38, 22)
for ($col = 0; $col -lt 5; $col++) {
    $x = 60 + ($col * 235)
    $g.DrawString(@("Monday", "Tuesday", "Wednesday", "Thursday", "Friday")[$col], (Font 16 Bold), (Brush "#374151"), $x, 115)
    $g.DrawRectangle([System.Drawing.Pens]::LightGray, $x, 155, 205, 475)
}
$g.FillRectangle((Brush "#DBEAFE"), 530, 260, 205, 120)
$g.DrawString("14:00", (Font 17 Bold), (Brush "#1E40AF"), 550, 278)
$g.DrawString("Design review", (Font 18 Bold), (Brush "#1E3A8A"), 550, 320)
$g.FillRectangle((Brush "#FEF3C7"), 765, 430, 205, 105)
$g.DrawString("Project sync", (Font 18 Bold), (Brush "#92400E"), 785, 456)
$g.DrawString("with Maya", (Font 16), (Brush "#92400E"), 785, 492)
Save-Canvas $canvas "team-calendar.png"
