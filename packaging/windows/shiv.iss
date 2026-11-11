; Inno Setup script. Build with: iscc /DAppVersion=0.1.0 packaging\windows\shiv.iss
; Expects target\release\shiv.exe; writes dist\Shiv-<version>-x64-setup.exe.
#ifndef AppVersion
  #error Pass /DAppVersion=<version>
#endif

[Setup]
; Never change AppId: it is how upgrades and the uninstaller find this app.
AppId={{6F2B1D4A-3C57-4E0B-9A18-5D7C2E91B3F4}
AppName=Shiv
AppVersion={#AppVersion}
AppPublisher=Siebe
AppPublisherURL=https://github.com/Siebe-Uy/shiv
DefaultDirName={autopf}\Shiv
DefaultGroupName=Shiv
; Per-user by default (no admin prompt); the dialog offers an all-users install.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=..\..\dist
OutputBaseFilename=Shiv-{#AppVersion}-x64-setup
SetupIconFile=shiv.ico
UninstallDisplayIcon={app}\shiv.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
DisableProgramGroupPage=yes

[Files]
Source: "..\..\target\release\shiv.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Shiv"; Filename: "{app}\shiv.exe"
Name: "{autodesktop}\Shiv"; Filename: "{app}\shiv.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked

[Run]
Filename: "{app}\shiv.exe"; Description: "Start Shiv"; Flags: nowait postinstall skipifsilent
