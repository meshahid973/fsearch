; FSearch installer. No admin privileges or background services required.

#define AppVersion "0.2.0"

[Setup]
AppId={{C14B3BD9-94AC-4E96-B689-A8424727A533}
AppName=FSearch
AppVersion={#AppVersion}
AppVerName=FSearch {#AppVersion}
AppPublisher=FSearch contributors
AppPublisherURL=https://github.com/meshahid973/fsearch
AppSupportURL=https://github.com/meshahid973/fsearch/issues
AppUpdatesURL=https://github.com/meshahid973/fsearch/releases
DefaultDirName={localappdata}\Programs\FSearch
DefaultGroupName=FSearch
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\fsearch-launcher.exe
OutputDir=..\dist
OutputBaseFilename=FSearch-Setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
VersionInfoVersion={#AppVersion}

[Files]
Source: "..\target\release\fsearch.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\fsearch-launcher.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"; Flags: unchecked

[Icons]
Name: "{autoprograms}\FSearch"; Filename: "{app}\fsearch-launcher.exe"; WorkingDir: "{app}"
Name: "{autodesktop}\FSearch"; Filename: "{app}\fsearch-launcher.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\fsearch-launcher.exe"; Description: "Open FSearch"; Flags: nowait postinstall skipifsilent
