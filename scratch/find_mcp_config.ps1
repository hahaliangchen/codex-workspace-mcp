$codex_dir = Join-Path $env:USERPROFILE ".codex"
Write-Output "Searching in $codex_dir"
Get-ChildItem -Path $codex_dir -File -Recurse -ErrorAction SilentlyContinue | ForEach-Object {
    if ($_.FullName -like "*\node_modules\*" -or $_.FullName -like "*\.tmp\*" -or $_.FullName -like "*\.sandbox\*") {
        return
    }
    $content = Get-Content -Raw -Path $_.FullName -ErrorAction SilentlyContinue
    if ($content -and $content -match "codex_workspace_mcp") {
        Write-Output "Found in: $($_.FullName)"
    }
}
