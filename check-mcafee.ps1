# Check if McAfee is running and causing issues
# Run this to diagnose McAfee interference

Write-Host "`n╔═══════════════════════════════════════════════════╗" -ForegroundColor Cyan
Write-Host "║   Voice Bird - McAfee Diagnostic Tool           ║" -ForegroundColor Cyan
Write-Host "╚═══════════════════════════════════════════════════╝`n" -ForegroundColor Cyan

# Check if running as Administrator
$isAdmin = ([Security.Principal.WindowsPrincipal] [Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)

if (!$isAdmin) {
    Write-Host "⚠ Warning: Not running as Administrator" -ForegroundColor Yellow
    Write-Host "Some checks may be limited. Right-click PowerShell → Run as Administrator for full diagnostics`n" -ForegroundColor Yellow
}

# ===== Check McAfee Status =====
Write-Host "═══════════════════════════════════════════════════" -ForegroundColor Cyan
Write-Host "  McAfee Services Status" -ForegroundColor Cyan
Write-Host "═══════════════════════════════════════════════════`n" -ForegroundColor Cyan

$mcafeeServices = Get-Service | Where-Object { $_.Name -like "*McAfee*" -or $_.Name -like "*McShield*" }

if ($mcafeeServices.Count -eq 0) {
    Write-Host "✓ McAfee not detected (not installed or disabled)" -ForegroundColor Green
    $hasMcAfee = $false
} else {
    Write-Host "⚠ McAfee detected:" -ForegroundColor Yellow
    $mcafeeServices | Format-Table Name, Status, StartType -AutoSize
    $hasMcAfee = $true

    $runningServices = $mcafeeServices | Where-Object { $_.Status -eq "Running" }
    if ($runningServices.Count -gt 0) {
        Write-Host "⚠ $($runningServices.Count) McAfee service(s) currently running" -ForegroundColor Yellow
        Write-Host "These may interfere with WebSocket audio streaming`n" -ForegroundColor Yellow
    }
}

# ===== Check McAfee Processes =====
if ($hasMcAfee) {
    Write-Host "═══════════════════════════════════════════════════" -ForegroundColor Cyan
    Write-Host "  McAfee Processes" -ForegroundColor Cyan
    Write-Host "═══════════════════════════════════════════════════`n" -ForegroundColor Cyan

    $mcafeeProcesses = Get-Process | Where-Object { $_.Name -like "*McAfee*" -or $_.Name -like "*McShield*" -or $_.Name -like "*mfemms*" }

    if ($mcafeeProcesses.Count -gt 0) {
        $mcafeeProcesses | Format-Table Name, CPU, @{Name="Memory(MB)";Expression={[math]::Round($_.WorkingSet/1MB,2)}} -AutoSize
    } else {
        Write-Host "✓ No active McAfee processes" -ForegroundColor Green
    }
}

# ===== Check Windows Firewall =====
Write-Host "`n═══════════════════════════════════════════════════" -ForegroundColor Cyan
Write-Host "  Windows Firewall Rules" -ForegroundColor Cyan
Write-Host "═══════════════════════════════════════════════════`n" -ForegroundColor Cyan

try {
    $firewallRules = Get-NetFirewallRule -ErrorAction Stop | Where-Object {
        $_.DisplayName -like "*voice*bird*" -or
        $_.DisplayName -like "*cargo*" -or
        $_.DisplayName -like "*rustc*"
    }

    if ($firewallRules.Count -eq 0) {
        Write-Host "⚠ No firewall rules found for Voice Bird" -ForegroundColor Yellow
        Write-Host "Recommendation: Run add-windows-firewall-rules.ps1 as Administrator`n" -ForegroundColor Yellow
    } else {
        Write-Host "✓ Found $($firewallRules.Count) firewall rule(s):" -ForegroundColor Green
        $firewallRules | Format-Table DisplayName, Enabled, Direction, Action -AutoSize
    }
} catch {
    Write-Host "⚠ Cannot check firewall rules (need Administrator privileges)" -ForegroundColor Yellow
}

# ===== Check Voice Bird Executable =====
Write-Host "`n═══════════════════════════════════════════════════" -ForegroundColor Cyan
Write-Host "  Voice Bird Executable" -ForegroundColor Cyan
Write-Host "═══════════════════════════════════════════════════`n" -ForegroundColor Cyan

$projectRoot = "C:\Projects\voice_bird_desktop"
$debugExe = "$projectRoot\target\debug\voice_bird_desktop.exe"
$releaseExe = "$projectRoot\target\release\voice_bird_desktop.exe"

if (Test-Path $releaseExe) {
    $exeInfo = Get-Item $releaseExe
    Write-Host "✓ Release executable found" -ForegroundColor Green
    Write-Host "  Path: $releaseExe" -ForegroundColor Cyan
    Write-Host "  Size: $([math]::Round($exeInfo.Length/1MB,2)) MB" -ForegroundColor Cyan
    Write-Host "  Modified: $($exeInfo.LastWriteTime)" -ForegroundColor Cyan
    $exePath = $releaseExe
} elseif (Test-Path $debugExe) {
    $exeInfo = Get-Item $debugExe
    Write-Host "⚠ Only debug executable found" -ForegroundColor Yellow
    Write-Host "  Path: $debugExe" -ForegroundColor Cyan
    Write-Host "  Size: $([math]::Round($exeInfo.Length/1MB,2)) MB" -ForegroundColor Cyan
    Write-Host "  Recommendation: Build release version with 'cargo build --release'" -ForegroundColor Yellow
    $exePath = $debugExe
} else {
    Write-Host "✗ No executable found" -ForegroundColor Red
    Write-Host "  Run 'cargo build --release' to build the application`n" -ForegroundColor Yellow
    $exePath = $null
}

# ===== Check Network Connectivity =====
Write-Host "`n═══════════════════════════════════════════════════" -ForegroundColor Cyan
Write-Host "  Network Connectivity Test" -ForegroundColor Cyan
Write-Host "═══════════════════════════════════════════════════`n" -ForegroundColor Cyan

$server = "voice-bird-app-ebrln.ondigitalocean.app"

Write-Host "Testing DNS resolution for $server..." -ForegroundColor Yellow
try {
    $dns = Resolve-DnsName $server -ErrorAction Stop | Where-Object { $_.Type -eq "A" }
    if ($dns) {
        Write-Host "✓ DNS resolved: $($dns[0].IPAddress)" -ForegroundColor Green
        $serverIP = $dns[0].IPAddress

        # Test HTTPS connectivity
        Write-Host "`nTesting HTTPS connectivity (port 443)..." -ForegroundColor Yellow
        $tcpTest = Test-NetConnection -ComputerName $server -Port 443 -WarningAction SilentlyContinue

        if ($tcpTest.TcpTestSucceeded) {
            Write-Host "✓ TCP connection successful (port 443)" -ForegroundColor Green
        } else {
            Write-Host "✗ TCP connection failed (port 443)" -ForegroundColor Red
            Write-Host "  This indicates firewall/antivirus blocking" -ForegroundColor Yellow
        }

        # Try HTTP request
        Write-Host "`nTesting HTTPS request..." -ForegroundColor Yellow
        try {
            $response = Invoke-WebRequest -Uri "https://$server" -UseBasicParsing -TimeoutSec 5 -ErrorAction Stop
            Write-Host "✓ HTTPS request successful (Status: $($response.StatusCode))" -ForegroundColor Green
        } catch {
            Write-Host "⚠ HTTPS request failed: $($_.Exception.Message)" -ForegroundColor Yellow
            Write-Host "  This could indicate SSL inspection or deep packet inspection" -ForegroundColor Yellow
        }
    }
} catch {
    Write-Host "✗ DNS resolution failed: $($_.Exception.Message)" -ForegroundColor Red
    Write-Host "  Check your internet connection" -ForegroundColor Yellow
}

# ===== Recommendations =====
Write-Host "`n═══════════════════════════════════════════════════" -ForegroundColor Cyan
Write-Host "  Recommendations" -ForegroundColor Cyan
Write-Host "═══════════════════════════════════════════════════`n" -ForegroundColor Cyan

if ($hasMcAfee) {
    Write-Host "McAfee is installed and running. Recommended actions:" -ForegroundColor Yellow
    Write-Host ""
    Write-Host "1. Add executable to McAfee Real-Time Scanning exclusions:" -ForegroundColor White
    if ($exePath) {
        Write-Host "   $exePath" -ForegroundColor Cyan
    } else {
        Write-Host "   (Build the executable first)" -ForegroundColor Yellow
    }

    Write-Host "`n2. Add project directory to McAfee exclusions:" -ForegroundColor White
    Write-Host "   C:\Projects\voice_bird_desktop\" -ForegroundColor Cyan
    Write-Host "   C:\Users\$env:USERNAME\.cargo\" -ForegroundColor Cyan

    Write-Host "`n3. Add server domain to Web Protection exclusions:" -ForegroundColor White
    Write-Host "   voice-bird-app-ebrln.ondigitalocean.app" -ForegroundColor Cyan

    Write-Host "`n4. Grant Full Access in McAfee Firewall for:" -ForegroundColor White
    if ($exePath) {
        Write-Host "   $exePath" -ForegroundColor Cyan
    }

    Write-Host "`n5. Disable SSL Scanning for:" -ForegroundColor White
    Write-Host "   voice-bird-app-ebrln.ondigitalocean.app" -ForegroundColor Cyan

    Write-Host "`nSee MCAFEE_EXCLUSIONS.md for detailed step-by-step instructions`n" -ForegroundColor Yellow

    Write-Host "═══════════════════════════════════════════════════" -ForegroundColor Cyan
    Write-Host "  Quick Test: Temporarily Disable McAfee" -ForegroundColor Cyan
    Write-Host "═══════════════════════════════════════════════════`n" -ForegroundColor Cyan
    Write-Host "To test if McAfee is causing the issue:" -ForegroundColor White
    Write-Host "1. Right-click McAfee icon in system tray" -ForegroundColor White
    Write-Host "2. Select 'Exit' or 'Disable'" -ForegroundColor White
    Write-Host "3. Run Voice Bird Desktop" -ForegroundColor White
    Write-Host "4. If it works → McAfee was the problem" -ForegroundColor White
    Write-Host "5. Re-enable McAfee immediately after testing!" -ForegroundColor Yellow
} else {
    Write-Host "✓ McAfee not detected" -ForegroundColor Green
    Write-Host "If you're still experiencing issues:" -ForegroundColor White
    Write-Host "- Check other antivirus software (Norton, Avast, etc.)" -ForegroundColor White
    Write-Host "- Check corporate firewall/proxy settings" -ForegroundColor White
    Write-Host "- Review FIXES_SUMMARY.md for other potential issues" -ForegroundColor White
}

Write-Host "`n═══════════════════════════════════════════════════`n" -ForegroundColor Cyan
