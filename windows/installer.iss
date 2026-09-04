; Mixtapes Windows Installer (Inno Setup)
; Build with: iscc installer.iss

#define MyAppName "Mixtapes"
#define MyAppVersion "2026.04.09.0"
#define MyAppPublisher "pocoguy"
#define MyAppURL "https://github.com/m-obeid/Mixtapes"
#define MyAppExeName "Mixtapes.exe"
#define MyAppId "com.pocoguy.Muse"

[Setup]
AppId={#MyAppId}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
OutputBaseFilename=MixtapesSetup
Compression=lzma2/ultra64
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
SetupIconFile=mixtapes.ico
UninstallDisplayIcon={app}\Mixtapes.exe
WizardStyle=modern
DisableProgramGroupPage=yes
LicenseFile=..\LICENSE

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; Main executable and helpers
Source: "{#SourcePath}\Mixtapes.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourcePath}\windows\MixtapesBridge.exe"; DestDir: "{app}\windows"; Flags: ignoreversion
Source: "{#SourcePath}\windows\MixtapesLogin.exe"; DestDir: "{app}\windows"; Flags: ignoreversion
Source: "{#SourcePath}\windows\mixtapes.ico"; DestDir: "{app}\windows"; Flags: ignoreversion
Source: "{#SourcePath}\windows\fonts.conf"; DestDir: "{app}\windows"; Flags: ignoreversion
Source: "{#SourcePath}\windows\rustypipe-botguard.exe"; DestDir: "{app}\windows"; Flags: ignoreversion

; Debug launcher
Source: "{#SourcePath}\mixtapes-debug.bat"; DestDir: "{app}"; Flags: ignoreversion

; App source
Source: "{#SourcePath}\src\*"; DestDir: "{app}\src"; Flags: ignoreversion recursesubdirs

; Assets
Source: "{#SourcePath}\assets\*"; DestDir: "{app}\assets"; Flags: ignoreversion recursesubdirs

; Fonts
Source: "{#SourcePath}\fonts\*"; DestDir: "{app}\fonts"; Flags: ignoreversion skipifsourcedoesntexist

; MSYS2 Runtime
Source: "{#SourcePath}\runtime\bin\*"; DestDir: "{app}\runtime\bin"; Flags: ignoreversion
Source: "{#SourcePath}\runtime\lib\*"; DestDir: "{app}\runtime\lib"; Flags: ignoreversion recursesubdirs
Source: "{#SourcePath}\runtime\share\*"; DestDir: "{app}\runtime\share"; Flags: ignoreversion recursesubdirs
Source: "{#SourcePath}\runtime\ssl\*"; DestDir: "{app}\runtime\ssl"; Flags: ignoreversion recursesubdirs

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\windows\mixtapes.ico"; AppUserModelID: "{#MyAppId}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\windows\mixtapes.ico"; AppUserModelID: "{#MyAppId}"; Tasks: desktopicon
Name: "{group}\{#MyAppName} (Debug)"; Filename: "{app}\mixtapes-debug.bat"; IconFilename: "{app}\windows\mixtapes.ico"
Name: "{group}\Login Helper"; Filename: "{app}\windows\MixtapesLogin.exe"
Name: "{group}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"

[Registry]
; Register AppUserModelID for proper taskbar/SMTC identification
Root: HKCU; Subkey: "Software\Classes\AppUserModelId\{#MyAppId}"; ValueType: string; ValueName: "DisplayName"; ValueData: "{#MyAppName}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\AppUserModelId\{#MyAppId}"; ValueType: string; ValueName: "IconUri"; ValueData: "{app}\windows\mixtapes.ico"; Flags: uninsdeletekey

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "Launch {#MyAppName}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
Type: filesandordirs; Name: "{localappdata}\muse"
