; ============================================================
; MemHover - Inno Setup Installation Script
; Professional Windows Installer Configuration
; ============================================================

#define AppName      "MemHover"
#define AppVersion   "1.0.0"
#define AppPublisher "Crow-developers"
#define AppURL       "https://github.com/Hussein-Furaty/MemHover"
#define AppExeName   "memhover.exe"

[Setup]
AppId={{8F3A2C1D-4E7B-4F9A-A123-0D1E2F3A4B5C}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}/issues
AppUpdatesURL={#AppURL}/releases
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
AllowNoIcons=yes
; Icon of the installer itself
SetupIconFile=..\assets\icon.ico
; Compression settings for a compact installer
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
; Output directory and filename
OutputDir=..\dist
OutputBaseFilename=MemHover-Setup-v{#AppVersion}
; Minimum OS: Windows 10
MinVersion=10.0
; Request admin privileges for installation
PrivilegesRequired=admin
; Uninstall display icon
UninstallDisplayIcon={app}\{#AppExeName}

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
; Option to add a desktop shortcut (unchecked by default — keeps it clean)
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
; Option to launch the app automatically on Windows startup
Name: "startupentry"; Description: "Launch MemHover automatically when Windows starts"; GroupDescription: "Startup:"; Flags: checkedonce

[Files]
; The main executable
Source: "..\target\release\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
; Application icon (for display purposes)
Source: "..\assets\icon.ico"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
; Start Menu shortcut
Name: "{group}\{#AppName}"; Filename: "{app}\{#AppExeName}"; IconFilename: "{app}\icon.ico"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"
; Desktop shortcut (only if the user selected the task above)
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; IconFilename: "{app}\icon.ico"; Tasks: desktopicon

[Registry]
; Register the startup entry in the Windows Registry (if user selected the task)
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExeName}"""; Flags: uninsdeletevalue; Tasks: startupentry

[Run]
; Offer to launch the application immediately after installation
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(AppName, '&', '&&')}}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Gracefully terminate the process if it is running at the time of uninstallation
Filename: "taskkill.exe"; Parameters: "/f /im {#AppExeName}"; Flags: runhidden; RunOnceId: "KillMemHover"
