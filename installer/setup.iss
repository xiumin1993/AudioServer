; setup.iss - Inno Setup script for "PC Assistant AudioServer" (Windows).
;
; WHY THIS FILE IS ASCII-ONLY
;   Inno Setup 6 decodes a .iss as UTF-8 only when the file carries a BOM; without a
;   BOM the bytes are read with the active ANSI codepage, so Chinese text written here
;   can turn into mojibake in the wizard. The program itself is bilingual (rust-i18n,
;   en/zh); the wizard language comes from the .isl files listed in [Languages].
;
; WHAT THIS INSTALLER DELIBERATELY DOES NOT DO
;   * It installs NO driver and runs NO driver-install step. There is no .msi/.inf/
;     .sys/.dll payload in the package at all, and pack_installer.ps1 fails the build
;     if one ever appears. VB-CABLE / Unity Capture / OBS Virtual Camera stay external
;     downloads that the user installs himself: audioserver.exe detects them at
;     startup and, when one is missing, shows which one plus a link and refuses to
;     enter the main window. Nothing in this setup writes kernel/driver state.
;   * It does not touch services, startup entries, firewall rules, or any registry
;     key beyond the standard uninstall entry Inno itself creates.
;   * It does NOT overwrite an existing %APPDATA%\PCAssistant\config.json on upgrade
;     (onlyifdoesntexist), so tuned settings survive a re-install.
;   * Uninstall keeps %APPDATA%\PCAssistant (config = user settings, log = evidence
;     for a bug report). Delete that folder by hand for a full reset.
;
; BUILD - never run ISCC by hand, use the script (it also generates the default config):
;   powershell -NoProfile -ExecutionPolicy Bypass -File D:\code\AudioServer\pack_installer.ps1
;
; Preprocessor symbols expected from pack_installer.ps1 (via /Dname=value):
;   MyAppVersion  display/file-name version, e.g. v3.4.11
;   MyVerNum      same without the leading v (Windows file-version fields need digits)
;   MyCommit      short git sha, written into the setup log
;   StageDir      folder holding the built files to install
;   InnoDir       where Inno Setup itself lives (to probe for a Chinese .isl file)

#define MyAppName "PC Assistant AudioServer"
#define MyAppPublisher "PC Assistant"
#ifndef MyAppVersion
  #define MyAppVersion "0.0.0-dev"
#endif
#ifndef MyVerNum
  #define MyVerNum "0.0.0.0"
#endif
#ifndef StageDir
  #define StageDir "..\dist\stage-installer"
#endif
#ifndef InnoDir
  #define InnoDir "."
#endif

[Setup]
; A stable AppId is what turns a second run of this setup into an UPGRADE instead of a
; second, side-by-side installation. Never change this GUID once builds are public.
AppId={{8B4E1D7A-5C39-4F1B-9E2D-7A63F0C4B118}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
AppUpdatesURL=https://github.com/xiumin1993/AudioServer
AppSupportURL=https://github.com/xiumin1993/AudioServer
VersionInfoVersion={#MyVerNum}
VersionInfoProductVersion={#MyVerNum}
VersionInfoDescription={#MyAppName} Setup
; 64-bit only build; excluding other CPUs up front beats a confusing loader error.
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; {autopf} = Program Files when the user picks "install for all users" (that path is
; the only one that ever asks for UAC), %LOCALAPPDATA%\Programs for a per-user install.
; PrivilegesRequiredOverridesAllowed hands the choice to the user in the wizard and on
; the command line (/ALLUSERS, /CURRENTUSER) - we never silently demand elevation.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog commandline
DefaultDirName={autopf}\PCAssistant
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\audioserver.exe
UninstallDisplayName={#MyAppName}
OutputDir=..\dist
OutputBaseFilename=AudioServer-{#MyAppVersion}-win-x64-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern

[Languages]
; English is always available. Chinese is wired in only when a ChineseSimplified.isl
; actually exists next to ISCC.exe (it is an unofficial translation, Inno does not
; bundle it) - then the wizard follows the system UI language on its own. We never
; download it as part of a build.
Name: "english"; MessagesFile: "compiler:Default.isl"
#if FileExists(AddBackslash(InnoDir) + "Languages\ChineseSimplified.isl")
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
#endif

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Additional icons:"; Flags: unchecked

[Files]
; GUI server = the one users are meant to run; CLI server is for scripting and CI.
Source: "{#StageDir}\audioserver.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\server.exe";      DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\README.md";       DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\quick-start.md";  DestDir: "{app}"; Flags: ignoreversion
Source: "{#StageDir}\VERSION.txt";     DestDir: "{app}"; Flags: ignoreversion
; The shipped starting-point config, generated at pack time by
;   server.exe --print-default-config
; so the file the user gets can never drift away from the defaults compiled into the
; exe. DestName renames it to config.json; onlyifdoesntexist = "give the user a
; config.json, never clobber one they already edited".
Source: "{#StageDir}\config.default.json"; DestDir: "{userappdata}\PCAssistant"; DestName: "config.json"; Flags: onlyifdoesntexist

[Dirs]
; The app creates this folder itself on first run; making it here too means the config
; file above always has a home, even on a profile where the first write would race.
Name: "{userappdata}\PCAssistant"

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\audioserver.exe"; WorkingDir: "{app}"; Comment: "Run PC Assistant AudioServer (phone as PC speaker / microphone / webcam)"
Name: "{group}\Config file (config.json)"; Filename: "{userappdata}\PCAssistant\config.json"
Name: "{group}\Quick start"; Filename: "{app}\quick-start.md"
Name: "{group}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\audioserver.exe"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
; Offered, never forced: postinstall = a checkbox on the last wizard page,
; skipifsilent = an unattended install does not pop a window.
Filename: "{app}\audioserver.exe"; Description: "Launch {#MyAppName} now"; Flags: postinstall nowait skipifsilent

[UninstallDelete]
; Only what this installer could have left inside {app}. In a portable layout the log
; file sits next to the exe; if the exe lives in Program Files the app writes it into
; %APPDATA%\PCAssistant instead (see main.rs log_file_path), and that folder survives
; on purpose.
Type: files; Name: "{app}\audioserver.log"

[Code]
// Write the resolved paths into the setup log: "which folder, which config, and the
// fact that no driver was installed" is the first thing a support thread needs.
procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    Log('PC Assistant AudioServer {#MyAppVersion} installed to ' + ExpandConstant('{app}'));
    Log('Config file: ' + ExpandConstant('{userappdata}\PCAssistant\config.json'));
    Log('No driver was installed by this setup. VB-CABLE / Unity Capture / OBS Virtual');
    Log('Camera must be installed by the user; the app detects them and links to them.');
  end;
end;
