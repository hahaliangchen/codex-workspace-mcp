# 终止运行中的 codex-workspace-mcp 进程以释放文件占用
Write-Host "Checking for running codex-workspace-mcp processes..." -ForegroundColor Cyan
$processes = Get-Process -Name "codex-workspace-mcp" -ErrorAction SilentlyContinue

if ($processes) {
    Write-Host "Stopping running processes..." -ForegroundColor Yellow
    $processes | Stop-Process -Force
    # 稍微等待一秒，确保系统释放句柄
    Start-Sleep -Seconds 1
} else {
    Write-Host "No running codex-workspace-mcp process found." -ForegroundColor Green
}

# 开始打包编译
Write-Host "Running cargo build --release..." -ForegroundColor Cyan
cargo build --release
