' Windowless launcher for the scheduled relay watchdog.
' Task Scheduler starts wscript.exe, a GUI-subsystem host, so no console window is
' allocated at all. Run(..., 0, True) starts PowerShell hidden and waits for it.
' This exists because a powershell.exe task action flashes a Windows Terminal
' window on this host (default terminal handoff), and that window can linger.

Option Explicit

Dim fso, sh, here, script, cmd
Set fso = CreateObject("Scripting.FileSystemObject")
Set sh = CreateObject("WScript.Shell")

here = fso.GetParentFolderName(WScript.ScriptFullName)
script = fso.BuildPath(here, "relay-watchdog.ps1")

cmd = "powershell.exe -NoProfile -ExecutionPolicy Bypass -NonInteractive -WindowStyle Hidden -File """ & script & """"
WScript.Quit sh.Run(cmd, 0, True)
