; Mixtapes Windows Installer (Inno Setup)
; Build with: iscc installer.iss

#define MyAppName "Mixtapes"
; CI passes the newest release from com.pocoguy.Muse.metainfo.xml with /DMyAppVersion=.
#ifndef MyAppVersion
  #define MyAppVersion "0.0.0.0"
#endif
#define MyAppPublisher "pocoguy"
#define MyAppURL "https://github.com/m-obeid/Mixtapes"
#define MyAppExeName "bin\mixtapes.exe"
#define MyAppId "com.pocoguy.Muse"

[Setup]
AppId={#MyAppId}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
; Windows file versions take dots only: 2026-10-31.1 becomes 2026.10.31.1.
VersionInfoVersion={#StringChange(MyAppVersion, "-", ".")}
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
UninstallDisplayIcon={app}\{#MyAppExeName}
WizardStyle=modern
DisableProgramGroupPage=yes
LicenseFile=..\LICENSE

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
; dist/mixtapes from windows/bundle.sh: bin, lib, libexec and share
Source: "{#SourcePath}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs
; The sign-in page needs the WebView2 runtime, which Windows 10 LTSC and older
; Windows 10 builds lack. CI downloads Microsoft's bootstrapper next to this file.
Source: "MicrosoftEdgeWebview2Setup.exe"; DestDir: "{tmp}"; Flags: deleteafterinstall; Check: NeedsWebView2

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; AppUserModelID: "{#MyAppId}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; AppUserModelID: "{#MyAppId}"; Tasks: desktopicon
Name: "{group}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"

[Registry]
; Register AppUserModelID for proper taskbar/SMTC identification
Root: HKCU; Subkey: "Software\Classes\AppUserModelId\{#MyAppId}"; ValueType: string; ValueName: "DisplayName"; ValueData: "{#MyAppName}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\AppUserModelId\{#MyAppId}"; ValueType: string; ValueName: "IconUri"; ValueData: "{app}\bin\mixtapes.ico"; Flags: uninsdeletekey

; mixtapes:// links open the app, which hands them to the running window.
Root: HKCU; Subkey: "Software\Classes\mixtapes"; ValueType: string; ValueName: ""; ValueData: "URL:Mixtapes link"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\mixtapes"; ValueType: string; ValueName: "URL Protocol"; ValueData: ""
Root: HKCU; Subkey: "Software\Classes\mixtapes\DefaultIcon"; ValueType: string; ValueName: ""; ValueData: "{app}\{#MyAppExeName}"
Root: HKCU; Subkey: "Software\Classes\mixtapes\shell\open\command"; ValueType: string; ValueName: ""; ValueData: """{app}\{#MyAppExeName}"" ""%1"""

[Run]
Filename: "{tmp}\MicrosoftEdgeWebview2Setup.exe"; Parameters: "/silent /install"; StatusMsg: "Installing the Microsoft Edge WebView2 Runtime..."; Flags: waituntilterminated; Check: NeedsWebView2
Filename: "{app}\{#MyAppExeName}"; Description: "Launch {#MyAppName}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
Type: filesandordirs; Name: "{localappdata}\muse"

[Code]
const
  WebView2Client = 'Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}';

{ Installed per machine or per user, the runtime records its version as pv. }
function HasWebView2Version(Root: Integer; Key: String): Boolean;
var
  Version: String;
begin
  Result := RegQueryStringValue(Root, Key, 'pv', Version) and (Version <> '') and (Version <> '0.0.0.0');
end;

function NeedsWebView2: Boolean;
begin
  Result := not (HasWebView2Version(HKLM, 'SOFTWARE\WOW6432Node\' + WebView2Client)
    or HasWebView2Version(HKLM, 'SOFTWARE\' + WebView2Client)
    or HasWebView2Version(HKCU, 'Software\' + WebView2Client));
end;
