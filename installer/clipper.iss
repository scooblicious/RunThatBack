; Inno Setup script for Clipper. Build with build_installer.bat (or open in Inno Setup and press F9).

#ifndef AppVersion
  #define AppVersion "0.4.0"
#endif
#define AppName "Clipper"
#define AppExe "clipper.exe"

[Setup]
AppId={{6F1D2B6A-3C54-4E0B-9A67-5B2C8E41D7A3}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppName}
; Per-user install: no admin prompt, and Clipper can write its settings file next to the exe.
PrivilegesRequired=lowest
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
OutputDir=Output
OutputBaseFilename=Clipper-Setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\{#AppExe}
UninstallDisplayName={#AppName}

[Tasks]
Name: "startup"; Description: "Start Clipper when I sign in to Windows"
Name: "desktopicon"; Description: "Create a &desktop shortcut"; Flags: unchecked

[Files]
Source: "..\target\release\clipper.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "ffmpeg.exe"; DestDir: "{app}"; Flags: ignoreversion
; default settings, but never overwrite the user's own on upgrade
Source: "..\clipper.cfg"; DestDir: "{app}"; Flags: onlyifdoesntexist

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExe}"
Name: "{autoprograms}\{#AppName} Clips"; Filename: "{app}\{#AppExe}"; Parameters: "--gallery"
Name: "{autoprograms}\{#AppName} Settings"; Filename: "{app}\{#AppExe}"; Parameters: "--gallery --settings"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExe}"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "{#AppName}"; ValueData: """{app}\{#AppExe}"""; Flags: uninsdeletevalue; Tasks: startup

[Run]
Filename: "{app}\{#AppExe}"; Description: "Launch {#AppName} now"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; settings are created by the app, not the installer; recorded clips are never touched
Type: files; Name: "{app}\clipper.cfg"
Type: dirifempty; Name: "{app}"

[Code]
procedure StopClipper;
var
  Code: Integer;
begin
  { /t also stops the ffmpeg child that Clipper started }
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/f /t /im {#AppExe}', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Sleep(600);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  StopClipper;
  Result := '';
end;

function InitializeUninstall(): Boolean;
begin
  StopClipper;
  Result := True;
end;
