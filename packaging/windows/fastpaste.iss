; Inno Setup 6 script for the per-user Windows installer. The release
; workflow (.github/workflows/release.yml) builds it with
;
;   iscc /DAppVersion=X.Y.Z /DStageDir=<dir> /DOutDir=<dir> fastpaste.iss
;
; where StageDir holds fastpaste.exe and LICENSE.txt, the same files that
; go into the portable zip.
;
; Per user means no UAC prompt: the app goes under %LOCALAPPDATA%\Programs,
; and the Start-menu shortcut, the autostart entry and the uninstaller's
; registration are all the current user's. Uninstalling leaves the
; settings and the snippet database alone.
;
; Code signing is not set up. Inno's SignTool= directive would sign both
; the installer and the uninstaller; see the stub in release.yml.

#ifndef AppVersion
  #error Pass the version: iscc /DAppVersion=X.Y.Z
#endif
#ifndef StageDir
  #error Pass the folder holding fastpaste.exe: iscc /DStageDir=...
#endif
#ifndef OutDir
  #define OutDir "Output"
#endif

; The version resource takes only numbers: 1.2.0-rc.1 becomes 1.2.0 there.
#define DashPos Pos("-", AppVersion)
#if DashPos > 0
  #define NumericVersion Copy(AppVersion, 1, DashPos - 1)
#else
  #define NumericVersion AppVersion
#endif

[Setup]
; Identifies the app across versions, so an upgrade replaces the old
; install instead of adding a second one. Never change it.
AppId={{F12D08CD-55F2-4803-9016-BBF7E725189F}
AppName=fastpaste
AppVersion={#AppVersion}
AppPublisher=The fastpaste authors
AppPublisherURL=https://github.com/stdex/fastpaste-rs
AppSupportURL=https://github.com/stdex/fastpaste-rs/issues
AppUpdatesURL=https://github.com/stdex/fastpaste-rs/releases
AppCopyright=Copyright (c) 2026 The fastpaste authors
VersionInfoVersion={#NumericVersion}
VersionInfoProductName=fastpaste
VersionInfoDescription=fastpaste Setup
PrivilegesRequired=lowest
DefaultDirName={autopf}\fastpaste
DisableProgramGroupPage=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Rust's MSVC target supports Windows 10 and later.
MinVersion=10.0
OutputDir={#OutDir}
OutputBaseFilename=fastpaste-{#AppVersion}-windows-x64-setup
SetupIconFile=fastpaste.ico
UninstallDisplayIcon={app}\fastpaste.exe
UninstallDisplayName=fastpaste
WizardStyle=modern
; Asks only when the Windows display language is none of these.
ShowLanguageDialog=auto
Compression=lzma2
SolidCompression=yes

; The app's own languages that Inno Setup ships a translation for.
[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "de"; MessagesFile: "compiler:Languages\German.isl"
Name: "es"; MessagesFile: "compiler:Languages\Spanish.isl"
Name: "ru"; MessagesFile: "compiler:Languages\Russian.isl"

[Tasks]
; Inno's own translated captions ("Automatically start fastpaste").
Name: "autostart"; Description: "{cm:AutoStartProgram,fastpaste}"; GroupDescription: "{cm:AutoStartProgramGroupDescription}"; Flags: unchecked

[Files]
Source: "{#StageDir}\fastpaste.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\fastpaste"; Filename: "{app}\fastpaste.exe"

[Registry]
; Start with Windows. The second line makes the checkbox authoritative on
; an upgrade: unticking it removes an entry an earlier install made.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "fastpaste"; ValueData: """{app}\fastpaste.exe"""; Flags: uninsdeletevalue; Tasks: autostart
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "fastpaste"; Flags: deletevalue; Tasks: not autostart

[Run]
Filename: "{app}\fastpaste.exe"; Description: "{cm:LaunchProgram,fastpaste}"; Flags: nowait postinstall skipifsilent
