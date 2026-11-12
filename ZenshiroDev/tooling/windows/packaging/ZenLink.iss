; ZenLink v2.5 installer (Inno Setup 6). Built by build_portable_release.ps1.
; Same AppId, folder and firewall rule as ZenLink v2.2.x: v2.5 is its successor
; and upgrades an installed v2.2.x in place (one "ZenLink" in Start, one entry
; in Apps). v2.5 keeps its own data folder (%LOCALAPPDATA%\ZenLink-Next) and
; ports, so a v2.2.31 Portable.exe still runs next to it. To roll back, run the
; v2.2.31 Setup.exe from its GitHub release.
#ifndef AppVersion
  #define AppVersion "2.5.1"
#endif
#ifndef AppVersionNum
  ; Numeric part only: VersionInfoVersion must be n.n.n.n.
  #define AppVersionNum "2.5.1"
#endif
#ifndef SourceDir
  #define SourceDir "..\..\..\..\out\dist\ZenLink"
#endif

[Setup]
AppId={{6B0E8C1A-5D3F-4E0A-9C2B-2E7A1F5D9A10}
AppName=ZenLink
AppVersion={#AppVersion}
AppVerName=ZenLink {#AppVersion}
AppPublisher=Zenvy Zenshiro
AppPublisherURL=https://github.com/zenvyzenshiro/zenlink
AppSupportURL=https://github.com/zenvyzenshiro/zenlink/issues
AppCopyright=Copyright (C) 2026 Zenvy Zenshiro
DefaultDirName={autopf}\ZenLink
DefaultGroupName=ZenLink
DisableProgramGroupPage=yes
LicenseFile=..\..\..\..\LICENSE
SetupIconFile=..\..\..\resources\assets\zenlink.ico
UninstallDisplayIcon={app}\ZenLink.exe
OutputBaseFilename=ZenLink-v{#AppVersion}-Setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
MinVersion=10.0.17763
CloseApplications=yes
; Setup writes its own log; it is copied to the app's log folder at the end.
SetupLogging=yes
VersionInfoCompany=Zenvy Zenshiro
VersionInfoDescription=ZenLink Setup
VersionInfoVersion={#AppVersionNum}
; Language page first: Windows' display language picks the default
; (Indonesian on an Indonesian Windows, English otherwise).
ShowLanguageDialog=yes
LanguageDetectionMethod=uilanguage

[Languages]
; English first: the fallback when Windows uses another language.
Name: "english"; MessagesFile: "compiler:Default.isl"
; Inno Setup's Indonesian translation (unofficial set), kept next to this file.
Name: "indonesian"; MessagesFile: "Indonesian.isl"

[CustomMessages]
english.FirewallTask=Allow ZenLink on the local network (needed for AirPlay, iPhone/iPad and Android over Wi-Fi; asks for administrator permission once)
indonesian.FirewallTask=Izinkan ZenLink di jaringan lokal (perlu untuk AirPlay, iPhone/iPad dan Android lewat Wi-Fi; minta izin administrator sekali)
english.NetworkGroup=Network:
indonesian.NetworkGroup=Jaringan:
english.AppleDevices=Install Apple Devices from the Microsoft Store (needed for Apple Driver mode)
indonesian.AppleDevices=Pasang Apple Devices dari Microsoft Store (perlu untuk mode Apple Driver)
english.LogsName=ZenLink logs
indonesian.LogsName=Log ZenLink
english.UninstallName=Uninstall ZenLink
indonesian.UninstallName=Hapus ZenLink
english.DonateButton=Support
indonesian.DonateButton=Donate

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"
Name: "firewall"; Description: "{cm:FirewallTask}"; GroupDescription: "{cm:NetworkGroup}"

[InstallDelete]
; An upgrade from v2.2.x (its libraries sit directly in {app}): remove what
; v2.5 no longer ships, measured by comparing both build folders.
Type: filesandordirs; Name: "{app}\av"
Type: filesandordirs; Name: "{app}\av.libs"
Type: filesandordirs; Name: "{app}\PIL"
Type: filesandordirs; Name: "{app}\webview"
Type: filesandordirs; Name: "{app}\pythonnet"
Type: filesandordirs; Name: "{app}\clr_loader"
Type: filesandordirs; Name: "{app}\chardet"
Type: filesandordirs; Name: "{app}\pydantic_core"
Type: filesandordirs; Name: "{app}\setuptools"
Type: filesandordirs; Name: "{app}\ui"
Type: filesandordirs; Name: "{app}\pywebview-*.dist-info"
Type: filesandordirs; Name: "{app}\typer-*.dist-info"
Type: filesandordirs; Name: "{app}\sounddevice-*.dist-info"
Type: files; Name: "{app}\sqlite3.dll"
Type: files; Name: "{app}\_sqlite3.pyd"
Type: files; Name: "{app}\_sounddevice_data\portaudio-binaries\*"
Type: filesandordirs; Name: "{app}\_sounddevice_data\portaudio-binaries\.github"
Type: files; Name: "{app}\_sounddevice_data\__init__.py"

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Excludes: "Start ZenLink.bat,run.bat,Create-Desktop-Shortcut.bat,Uninstall-ZenLink.bat,portable.txt"; Flags: ignoreversion recursesubdirs createallsubdirs

[Dirs]
Name: "{localappdata}\ZenLink-Next\logs"

[Icons]
Name: "{group}\ZenLink"; Filename: "{app}\ZenLink.exe"
Name: "{group}\{cm:UninstallName}"; Filename: "{uninstallexe}"
; The log folder (window, backend, crash and setup logs) for bug reports.
Name: "{group}\{cm:LogsName}"; Filename: "{localappdata}\ZenLink-Next\logs"
; Also in the install folder (Program Files is read-only, so the logs live in
; %LOCALAPPDATA%; this shortcut leads there from where ZenLink is installed).
Name: "{app}\Logs"; Filename: "{localappdata}\ZenLink-Next\logs"
Name: "{autodesktop}\ZenLink"; Filename: "{app}\ZenLink.exe"; Tasks: desktopicon

[Run]
; AirPlay (ZenLink Studio over Wi-Fi): the iPhone/iPad must reach ZenLink. Inbound allow, local subnet only.
; adb is no longer bundled (ZenLink downloads it on request); the same elevated command removes
; the "ZenLink (ADB)" rule that earlier versions added.
Filename: "{sys}\cmd.exe"; Parameters: "/c netsh advfirewall firewall delete rule name=""ZenLink (AirPlay)"" program=""{app}\ZenLinkService.exe"" & netsh advfirewall firewall delete rule name=""ZenLink (AirPlay)"" program=""{app}\ZenLink.exe"" & netsh advfirewall firewall add rule name=""ZenLink (AirPlay)"" dir=in action=allow program=""{app}\ZenLink.exe"" enable=yes profile=any remoteip=localsubnet & netsh advfirewall firewall delete rule name=""ZenLink (ADB)"" program=""{app}\third_party\platform-tools\adb.exe"""; Flags: runhidden shellexec waituntilterminated; Verb: runas; Tasks: firewall
; Apple Driver mode needs Apple Mobile Device Service (Apple Devices or iTunes).
Filename: "ms-windows-store://pdp/?ProductId=9NP83LWLPZ9K"; Description: "{cm:AppleDevices}"; Flags: postinstall shellexec skipifsilent unchecked; Check: not AppleServiceInstalled
Filename: "{app}\ZenLink.exe"; Description: "{cm:LaunchProgram,ZenLink}"; Flags: nowait postinstall skipifsilent
; In-app update (v2.5.1): ZenLink runs this Setup with /SILENT /ZLRELAUNCH=1 and
; quits; start the new version when the silent install is done.
Filename: "{app}\ZenLink.exe"; Flags: nowait; Check: RelaunchAfterUpdate

[UninstallRun]
; Stop only the processes started from this install folder: a portable ZenLink uses the same exe names.
Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -Command ""Get-Process ZenLink,ZenLinkService -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -like '{app}\*' } | Stop-Process -Force"""; Flags: runhidden; RunOnceId: "StopZenLink"
Filename: "{sys}\cmd.exe"; Parameters: "/c netsh advfirewall firewall delete rule name=""ZenLink (AirPlay)"" program=""{app}\ZenLinkService.exe"" & netsh advfirewall firewall delete rule name=""ZenLink (AirPlay)"" program=""{app}\ZenLink.exe"" & netsh advfirewall firewall delete rule name=""ZenLink (ADB)"" program=""{app}\third_party\platform-tools\adb.exe"""; Flags: runhidden shellexec waituntilterminated; Verb: runas; RunOnceId: "DelFirewall"; Check: FirewallRuleExists

[UninstallDelete]
Type: filesandordirs; Name: "{localappdata}\ZenLink-Next"

[Registry]
; "Start with Windows" (Settings) writes this value itself; remove it on uninstall.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "ZenLink"; Flags: dontcreatekey uninsdeletevalue

[Code]
// Only ask for administrator rights on uninstall when a ZenLink firewall rule exists.
function FirewallRuleExists(): Boolean;
var
  Names: TArrayOfString;
  I: Integer;
  Value: String;
begin
  Result := False;
  if RegGetValueNames(HKLM, 'SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\FirewallRules', Names) then
    for I := 0 to GetArrayLength(Names) - 1 do
      if RegQueryStringValue(HKLM, 'SYSTEM\CurrentControlSet\Services\SharedAccess\Parameters\FirewallPolicy\FirewallRules', Names[I], Value) then
        if (Pos('|Name=ZenLink (AirPlay)|', Value) > 0) or (Pos('|Name=ZenLink (ADB)|', Value) > 0) then
        begin
          Result := True;
          Exit;
        end;
end;

function AppleServiceInstalled(): Boolean;
var
  ImagePath: String;
begin
  Result := RegQueryStringValue(HKLM, 'SYSTEM\CurrentControlSet\Services\Apple Mobile Device Service', 'ImagePath', ImagePath);
end;

function RelaunchAfterUpdate(): Boolean;
begin
  Result := WizardSilent() and (ExpandConstant('{param:ZLRELAUNCH|0}') = '1');
end;

// Clean update (v2.5.1): install-manifest.txt lists the top-level files and
// folders the previous ZenLink installed in {app}. Remove exactly those before
// the new files are copied, so nothing a newer version no longer ships stays
// behind. Nothing else in {app} is touched (a user may have picked a folder
// with other files), and settings live in %APPDATA% / %LOCALAPPDATA%.
procedure RemovePreviousFiles();
var
  Lines: TArrayOfString;
  I: Integer;
  Name, Path: String;
begin
  if not LoadStringsFromFile(ExpandConstant('{app}\install-manifest.txt'), Lines) then
    Exit;
  for I := 0 to GetArrayLength(Lines) - 1 do
  begin
    Name := Trim(Lines[I]);
    if (Name = '') or (Name = '.') or (Pos('..', Name) > 0) or (Pos('', Name) > 0) or (Pos('/', Name) > 0) or (Pos(':', Name) > 0) then
      Continue;
    if CompareText(Copy(Name, 1, 5), 'unins') = 0 then
      Continue;
    Path := ExpandConstant('{app}') + Name;
    if DirExists(Path) then
      DelTree(Path, True, True, True)
    else if FileExists(Path) then
      DeleteFile(Path);
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssInstall then
    RemovePreviousFiles();
  if CurStep = ssDone then
    // Keep the setup log next to the app's own logs (best effort).
    FileCopy(ExpandConstant('{log}'), ExpandConstant('{localappdata}\ZenLink-Next\logs\setup.log'), False);
end;

function InitializeSetup(): Boolean;
begin
  // No prerequisite check: the Rust window does not use Microsoft Edge
  // WebView2 (only the old classic window did). See B-6.
  Result := True;
end;

// Finish page: a Donate / Support button left of Finish (the Sociabuzz page
// from README and the website). Hidden on other pages and in silent updates.
var
  DonateButton: TNewButton;

procedure DonateClick(Sender: TObject);
var
  Code: Integer;
begin
  ShellExecAsOriginalUser('open', 'https://sociabuzz.com/zenvy/tribe', '', '', SW_SHOWNORMAL, ewNoWait, Code);
end;

procedure InitializeWizard();
begin
  DonateButton := TNewButton.Create(WizardForm);
  DonateButton.Parent := WizardForm;
  DonateButton.Caption := CustomMessage('DonateButton');
  DonateButton.Width := ScaleX(110);
  DonateButton.Height := WizardForm.NextButton.Height;
  DonateButton.Top := WizardForm.NextButton.Top;
  // Same margin from the left edge as Cancel has from the right edge.
  DonateButton.Left := WizardForm.ClientWidth - WizardForm.CancelButton.Left - WizardForm.CancelButton.Width;
  DonateButton.Anchors := [akLeft, akBottom];
  DonateButton.OnClick := @DonateClick;
  DonateButton.Visible := False;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  DonateButton.Visible := (CurPageID = wpFinished) and not WizardSilent();
end;
