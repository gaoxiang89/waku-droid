; Narel's Windows installer (community fork of egoist/waku).
;
; Per-user by design: %LOCALAPPDATA%\Programs needs no elevation, which is
; what lets the in-app updater re-run this silently without a UAC prompt.
; See RELEASING.md and docs/windows.md.
;
; Built by scripts/bundle-windows.ts, which supplies:
;   /DAppVersion=<version>  /DArch=<x86_64|aarch64>
;   /DStageDir=<dir with the built executables>  /DOutputDir=<dir>
;   /DHasRemoteDaemons=<0|1> (optional; defaults to 0)

#ifndef AppVersion
  #error AppVersion must be defined (ISCC /DAppVersion=...)
#endif
#ifndef Arch
  #error Arch must be defined (ISCC /DArch=...)
#endif
#ifndef StageDir
  #error StageDir must be defined (ISCC /DStageDir=...)
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif
#ifndef HasRemoteDaemons
  #define HasRemoteDaemons 0
#endif

; An x64 build is worth allowing on Arm, where it runs emulated; an arm64
; build on x64 is not, so refuse it up front rather than installing something
; that cannot start.
#if Arch == "aarch64"
  #define Architectures "arm64"
#else
  #define Architectures "x64compatible"
#endif

[Setup]
; Never change AppId: it is how Windows and every later installer recognize
; an existing install, and how the updater replaces rather than duplicates it.
AppId={{D4F8A2C1-7B3E-4A9D-8E21-5C6F9A0B3E47}
AppName=Narel
AppVersion={#AppVersion}
VersionInfoVersion={#AppVersion}
AppPublisher=Narel
AppPublisherURL=https://github.com/gaoxiang89/waku-droid
AppSupportURL=https://github.com/gaoxiang89/waku-droid/issues
AppUpdatesURL=https://github.com/gaoxiang89/waku-droid/releases
DefaultDirName={autopf}\Narel
DefaultGroupName=Narel
UninstallDisplayName=Narel
UninstallDisplayIcon={app}\narel.exe
LicenseFile={#StageDir}\LICENSE
OutputDir={#OutputDir}
OutputBaseFilename=Narel-{#AppVersion}-{#Arch}-Setup
SetupIconFile=AppIcon.ico
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed={#Architectures}
ArchitecturesInstallIn64BitMode={#Architectures}
; What docs/windows.md promises. Enforcing it here beats installing onto a
; system that cannot run the result.
MinVersion=10.0.17763
; Two installers must not race — the updater can be triggered again while an
; update is already applying.
SetupMutex=NarelSetup
; No elevation, so an update never has to ask for it either.
PrivilegesRequired=lowest
DisableProgramGroupPage=yes
DisableReadyPage=yes
; The updater passes /DIR, and a manual reinstall should land where the
; previous one did rather than asking again. Preserve previous task choices
; (such as PATH registration) across reinstalls and silent updates.
UsePreviousAppDir=yes
UsePreviousTasks=yes
; Broadcast WM_SETTINGCHANGE so new shells see PATH changes immediately.
ChangesEnvironment=yes
; Narel persists continuously to SQLite, so closing it is safe; a silent
; update cannot stop to ask, and a locked narel.exe would fail the install.
CloseApplications=force
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Files]
Source: "{#StageDir}\narel.exe"; DestDir: "{app}"; Flags: ignoreversion
; Console-subsystem CLI binary; `{app}\bin` is what gets registered on PATH.
Source: "{#StageDir}\bin\narel.exe"; DestDir: "{app}\bin"; Flags: ignoreversion
Source: "{#StageDir}\narel-daemon.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\narel_js_repl.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\narel_computer_use.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\cua_driver_sdk.dll"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\cua-driver-uia.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\resources\*"; DestDir: "{app}\resources"; Flags: ignoreversion recursesubdirs createallsubdirs
; Linux daemons for WSL and SSH hosts. A local build may package without them.
Source: "{#StageDir}\remote\*"; DestDir: "{app}\remote"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#StageDir}\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

; ISCC /D defines values as strings, so Int(..., 0) handles both command-line
; "0"/"1" and the numeric default 0 uniformly.
#if Int(HasRemoteDaemons, 0) != 0
[InstallDelete]
Type: filesandordirs; Name: "{app}\remote"
#endif

[Icons]
Name: "{group}\Narel"; Filename: "{app}\narel.exe"
Name: "{userdesktop}\Narel"; Filename: "{app}\narel.exe"; Tasks: desktopicon

[Tasks]
Name: "addtopath"; Description: "Add Narel CLI (narel) to user PATH"
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; Flags: unchecked

[Run]
; No skipifsilent: this is also how the updater's silent run brings Narel back.
Filename: "{app}\narel.exe"; Description: "{cm:LaunchProgram,Narel}"; Flags: nowait postinstall

[Code]
const
  EnvironmentKey = 'Environment';
  REG_SZ = 1;
  RRF_RT_ANY = $0000FFFF;
  RRF_NOEXPAND = $10000000;

function RegGetValue(
  hKey: Integer;
  lpSubKey: string;
  lpValue: string;
  dwFlags: DWORD;
  var pdwType: DWORD;
  pvData: Cardinal;
  pcbData: Cardinal
): Longint;
external 'RegGetValueW@advapi32.dll stdcall';

function NormalizePathEntry(Entry: string): string;
begin
  Result := Trim(Entry);
  if (Length(Result) >= 2) and (Result[1] = '"') and (Result[Length(Result)] = '"') then
    Result := Trim(Copy(Result, 2, Length(Result) - 2));
  Result := RemoveBackslashUnlessRoot(Result);
  Result := Lowercase(Result);
end;

function IsWakuInstallDir(Dir: string): Boolean;
begin
  Result := FileExists(AddBackslash(Dir) + 'waku.exe');
end;

procedure InitializeWizard;
var
  TargetDir: string;
begin
  if not IsWakuInstallDir(WizardDirValue) then
    Exit;

  { Waku's updater passes its own folder in /DIR. Honor an existing Narel
    location if it is independent, otherwise use the Narel default. }
  TargetDir := WizardForm.PrevAppDir;
  if (TargetDir = '') or IsWakuInstallDir(TargetDir) then
    TargetDir := ExpandConstant('{autopf}\Narel');
  Log('Redirecting installation from Waku directory to ' + TargetDir);
  WizardForm.DirEdit.Text := TargetDir;
end;

function PrepareToInstall(var NeedsRestart: Boolean): string;
begin
  Result := '';
  if IsWakuInstallDir(WizardDirValue) then
    Result := 'Choose a separate folder for Narel. This folder contains a Waku installation.';
end;

function UserPathUsesExpandString(HasExisting: Boolean; ExistingPaths: string): Boolean;
var
  ValueType: DWORD;
begin
  // Default to REG_EXPAND_SZ for a newly created user Path, matching Windows.
  if not HasExisting then
  begin
    Result := True;
    Exit;
  end;
  // Query the stored registry type without expanding %VAR% so an existing
  // REG_EXPAND_SZ or REG_SZ value keeps its original type.
  ValueType := 0;
  if RegGetValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', RRF_RT_ANY or RRF_NOEXPAND, ValueType, 0, 0) = 0 then
    Result := ValueType <> REG_SZ
  else
    Result := Pos('%', ExistingPaths) > 0;
end;

procedure WriteUserPath(NewPaths: string; UseExpand: Boolean);
begin
  // PrivilegesRequired=lowest: only touch HKCU\Environment, never HKLM.
  if UseExpand then
  begin
    if not RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', NewPaths) then
      Log('Failed to write HKCU\Environment\Path (REG_EXPAND_SZ)');
  end
  else
  begin
    if not RegWriteStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', NewPaths) then
      Log('Failed to write HKCU\Environment\Path (REG_SZ)');
  end;
end;

procedure EnvAddPath(Path: string);
var
  CleanPath: string;
  NormalizedTarget: string;
  Paths: string;
  Remaining: string;
  Entry: string;
  NewPaths: string;
  Sep: Integer;
  HasExisting: Boolean;
  UseExpand: Boolean;
begin
  CleanPath := RemoveBackslashUnlessRoot(Trim(Path));
  NormalizedTarget := NormalizePathEntry(CleanPath);
  if NormalizedTarget = '' then
    Exit;

  HasExisting := RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths);
  if not HasExisting then
    Paths := '';

  UseExpand := UserPathUsesExpandString(HasExisting, Paths);

  Remaining := Paths;
  while Remaining <> '' do
  begin
    Sep := Pos(';', Remaining);
    if Sep > 0 then
    begin
      Entry := Copy(Remaining, 1, Sep - 1);
      Delete(Remaining, 1, Sep);
    end
    else
    begin
      Entry := Remaining;
      Remaining := '';
    end;
    if NormalizePathEntry(Entry) = NormalizedTarget then
      Exit;
  end;

  NewPaths := Paths;
  while (Length(NewPaths) > 0) and (NewPaths[Length(NewPaths)] = ';') do
    Delete(NewPaths, Length(NewPaths), 1);

  if NewPaths = '' then
    NewPaths := CleanPath
  else
    NewPaths := NewPaths + ';' + CleanPath;

  WriteUserPath(NewPaths, UseExpand);
end;

procedure EnvRemovePath(Path: string);
var
  NormalizedTarget: string;
  Paths: string;
  Remaining: string;
  Entry: string;
  NewPaths: string;
  Sep: Integer;
  Removed: Boolean;
  UseExpand: Boolean;
begin
  NormalizedTarget := NormalizePathEntry(Path);
  if NormalizedTarget = '' then
    Exit;

  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths) then
    Exit;

  UseExpand := UserPathUsesExpandString(True, Paths);

  Removed := False;
  NewPaths := '';
  Remaining := Paths;
  while Remaining <> '' do
  begin
    Sep := Pos(';', Remaining);
    if Sep > 0 then
    begin
      Entry := Copy(Remaining, 1, Sep - 1);
      Delete(Remaining, 1, Sep);
    end
    else
    begin
      Entry := Remaining;
      Remaining := '';
    end;
    if NormalizePathEntry(Entry) = NormalizedTarget then
      Removed := True
    else if Trim(Entry) <> '' then
    begin
      if NewPaths <> '' then
        NewPaths := NewPaths + ';';
      NewPaths := NewPaths + Entry;
    end;
  end;

  if not Removed then
    Exit;

  WriteUserPath(NewPaths, UseExpand);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and WizardIsTaskSelected('addtopath') then
    EnvAddPath(ExpandConstant('{app}\bin'));
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    EnvRemovePath(ExpandConstant('{app}\bin'));
end;
