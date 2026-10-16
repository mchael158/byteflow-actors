param(
    [int]$WatchPid,
    [string]$Root,
    [string]$Log
)

Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing

$notify = New-Object System.Windows.Forms.NotifyIcon
$notify.Icon = [System.Drawing.SystemIcons]::Information
$notify.Text = "memoria active"
$notify.Visible = $true

$menu = New-Object System.Windows.Forms.ContextMenuStrip
$openLog = New-Object System.Windows.Forms.ToolStripMenuItem("Open log")
$openLog.add_Click({
    if (Test-Path $Log) {
        Start-Process notepad.exe $Log
    }
})
$stop = New-Object System.Windows.Forms.ToolStripMenuItem("Stop memoria")
$stop.add_Click({
    taskkill /PID $WatchPid /T /F | Out-Null
    $notify.Visible = $false
    [System.Windows.Forms.Application]::Exit()
})
$menu.Items.Add($openLog) | Out-Null
$menu.Items.Add($stop) | Out-Null
$notify.ContextMenuStrip = $menu

$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 5000
$timer.add_Tick({
    if (-not (Get-Process -Id $WatchPid -ErrorAction SilentlyContinue)) {
        $notify.Visible = $false
        [System.Windows.Forms.Application]::Exit()
    }
})
$timer.Start()

[System.Windows.Forms.Application]::Run()
