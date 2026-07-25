; SensorView Windows Installer Script (Inno Setup)
; Bundles SensorView + LibreHardwareMonitor Sidecar + PawnIO Kernel Driver Setup

#define MyAppName "SensorView"
#define MyAppVersion "0.1.0"
#define MyAppPublisher "OpenHardwareMonitor Project"
#define MyAppURL "https://github.com/Zektopic/openhardwaremonitor"
#define MyAppExeName "sensorview.exe"

[Setup]
AppId={{D37B4890-4491-4F43-989D-8611F0B86F21}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
AppUpdatesURL={#MyAppURL}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
AllowNoIcons=yes
OutputDir=..\target\installer
OutputBaseFilename=SensorView_Setup_v{#MyAppVersion}
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=admin

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "pawnio"; Description: "Install PawnIO Driver (Recommended for Windows 11 HVCI / Memory Integrity support)"; GroupDescription: "Kernel Driver Setup:"; Flags: checkedonce

[Files]
Source: "..\target\release\sensorview.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\sensorview-bridge.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\sensorview-bridge.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\sensorview-bridge.deps.json"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\sensorview-bridge.runtimeconfig.json"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\resources\PawnIO_setup.exe"; DestDir: "{app}\resources"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Run]
; Silently install PawnIO driver if selected during wizard
Filename: "{app}\resources\PawnIO_setup.exe"; Parameters: "/VERYSILENT /NORESTART"; Tasks: pawnio; Flags: runascurrentuser waituntilterminated

; Option to run SensorView after setup completes
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#StringChange(MyAppName, '&', '&&')}}"; Flags: runascurrentuser postinstall nowait skipifsilent
