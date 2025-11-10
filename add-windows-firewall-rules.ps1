# Add Windows Firewall rules for Voice Bird Desktop
# Run as Administrator: Right-click PowerShell → Run as Administrator

param(
    [string]$ExePath = "C:\Projects\voice_bird_desktop\target\release\voice_bird_desktop.exe"
)

Write-Host "`n╔═══════════════════════════════════════════════════╗" -ForegroundColor Cyan
Write-Host "║   Voice Bird - Windows Firewall Configuration   ║" -ForegroundColor Cyan
Write-Host "╚═══════════════════════════════════════════════════╝`n" -ForegroundColor Cyan

# Check if running as Administrator
$isAdmin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

if (!$isAdmin) {
    Write-Host "✗ Error: Must run as Administrator" -ForegroundColor Red
    Write-Host "`nPlease:" -ForegroundColor Yellow
    Write-Host "1. Right-click PowerShell" -ForegroundColor Yellow
    Write-Host "2. Select 'Run as Administrator'" -ForegroundColor Yellow
    Write-Host "3. Run this script again`n" -ForegroundColor Yellow
    exit 1
}

# Check if executable exists
if (!(Test-Path $ExePath)) {
    Write-Host "✗ Executable not found at:" -ForegroundColor Red
    Write-Host "  $ExePath`n" -ForegroundColor Red

    # Check if debug version exists
    $debugPath = "C:\Projects\voice_bird_desktop\target\debug\voice_bird_desktop.exe"
    if (Test-Path $debugPath) {
        Write-Host "Found debug version instead:" -ForegroundColor Yellow
        Write-Host "  $debugPath`n" -ForegroundColor Cyan

        $response = Read-Host "Use debug version instead? (y/n)"
        if ($response -eq 'y' -or $response -eq 'Y') {
            $ExePath = $debugPath
        } else {
            Write-Host "`nPlease build the release version first:" -ForegroundColor Yellow
            Write-Host "  cd C:\Projects\voice_bird_desktop" -ForegroundColor Cyan
            Write-Host "  cargo build --release`n" -ForegroundColor Cyan
            exit 1
        }
    } else {
        Write-Host "Please build the application first:" -ForegroundColor Yellow
        Write-Host "  cd C:\Projects\voice_bird_desktop" -ForegroundColor Cyan
        Write-Host "  cargo build --release`n" -ForegroundColor Cyan
        exit 1
    }
}

Write-Host "✓ Executable found:" -ForegroundColor Green
Write-Host "  $ExePath`n" -ForegroundColor Cyan

# Remove existing rules (if any)
Write-Host "Removing existing Voice Bird firewall rules..." -ForegroundColor Yellow
try {
    Remove-NetFirewallRule -DisplayName "Voice Bird Desktop - Outbound" -ErrorAction SilentlyContinue
    Remove-NetFirewallRule -DisplayName "Voice Bird Desktop - Inbound" -ErrorAction SilentlyContinue
    Write-Host "✓ Old rules removed (if any existed)`n" -ForegroundColor Green
} catch {
    Write-Host "⚠ Warning: Could not remove old rules: $($_.Exception.Message)`n" -ForegroundColor Yellow
}

# Add outbound rule
Write-Host "Adding outbound firewall rule..." -ForegroundColor Yellow
try {
    New-NetFirewallRule -DisplayName "Voice Bird Desktop - Outbound" `
        -Direction Outbound `
        -Program $ExePath `
        -Action Allow `
        -Protocol TCP `
        -Enabled True `
        -Profile Any `
        -Description "Allow Voice Bird Desktop to send audio data via WebSocket to voice-bird-app-ebrln.ondigitalocean.app" `
        -ErrorAction Stop | Out-Null

    Write-Host "✓ Outbound rule added" -ForegroundColor Green
} catch {
    Write-Host "✗ Failed to add outbound rule: $($_.Exception.Message)" -ForegroundColor Red
    exit 1
}

# Add inbound rule
Write-Host "Adding inbound firewall rule..." -ForegroundColor Yellow
try {
    New-NetFirewallRule -DisplayName "Voice Bird Desktop - Inbound" `
        -Direction Inbound `
        -Program $ExePath `
        -Action Allow `
        -Protocol TCP `
        -Enabled True `
        -Profile Any `
        -Description "Allow Voice Bird Desktop to receive WebSocket responses from voice-bird-app-ebrln.ondigitalocean.app" `
        -ErrorAction Stop | Out-Null

    Write-Host "✓ Inbound rule added" -ForegroundColor Green
} catch {
    Write-Host "✗ Failed to add inbound rule: $($_.Exception.Message)" -ForegroundColor Red
    exit 1
}

# Verify rules were created
Write-Host "`nVerifying firewall rules..." -ForegroundColor Yellow
$rules = Get-NetFirewallRule | Where-Object { $_.DisplayName -like "Voice Bird Desktop*" }

if ($rules.Count -ge 2) {
    Write-Host "✓ Firewall rules configured successfully!`n" -ForegroundColor Green
    $rules | Format-Table DisplayName, Enabled, Direction, Action -AutoSize
} else {
    Write-Host "⚠ Warning: Expected 2 rules, found $($rules.Count)" -ForegroundColor Yellow
}

# Additional information
Write-Host "`n═══════════════════════════════════════════════════" -ForegroundColor Cyan
Write-Host "  Important Notes" -ForegroundColor Cyan
Write-Host "═══════════════════════════════════════════════════`n" -ForegroundColor Cyan

Write-Host "✓ Windows Firewall rules have been added" -ForegroundColor Green
Write-Host ""
Write-Host "What this does:" -ForegroundColor White
Write-Host "- Allows Voice Bird Desktop to connect to the server" -ForegroundColor White
Write-Host "- Allows WebSocket traffic on port 443 (HTTPS/WSS)" -ForegroundColor White
Write-Host "- Works with all network profiles (Public, Private, Domain)" -ForegroundColor White
Write-Host ""
Write-Host "What this DOES NOT do:" -ForegroundColor Yellow
Write-Host "- This only affects Windows Firewall" -ForegroundColor Yellow
Write-Host "- If you have McAfee, you STILL need to add exclusions there" -ForegroundColor Yellow
Write-Host "- See MCAFEE_EXCLUSIONS.md for McAfee-specific steps" -ForegroundColor Yellow
Write-Host ""
Write-Host "Next steps:" -ForegroundColor Cyan
Write-Host "1. If you have McAfee: Add exclusions (see MCAFEE_EXCLUSIONS.md)" -ForegroundColor White
Write-Host "2. Run Voice Bird Desktop: cargo run --release" -ForegroundColor White
Write-Host "3. Test audio streaming to the server" -ForegroundColor White
Write-Host ""
Write-Host "To remove these rules later:" -ForegroundColor Cyan
Write-Host "  Remove-NetFirewallRule -DisplayName 'Voice Bird Desktop*'" -ForegroundColor White
Write-Host ""
Write-Host "═══════════════════════════════════════════════════`n" -ForegroundColor Cyan
