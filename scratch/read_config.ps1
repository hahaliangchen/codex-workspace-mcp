$path = Join-Path $env:USERPROFILE ".codex\.codex-global-state.json"
Write-Output "Checking path: $path"
if (Test-Path $path) {
    $raw = Get-Content -Raw -Path $path
    if ($raw -match '"mcpServers"\s*:\s*(\{.+?\})') {
        Write-Output "Found mcpServers:"
        Write-Output $Matches[1]
    } else {
        Write-Output "No mcpServers regex match. Let's dump all keys containing mcp:"
        $json = $raw | ConvertFrom-Json
        $json.PSObject.Properties | Where-Object {$_.Name -like '*mcp*'} | ForEach-Object {
            Write-Output "$($_.Name): $($_.Value | ConvertTo-Json -Depth 5)"
        }
    }
} else {
    Write-Output "Path does not exist!"
}
