; Inno Setup script for the `scode` CLI.
;
; Builds a per-user installer that drops scode.exe into a directory and
; appends that directory to the user's PATH, so `scode` works from any
; new terminal after install. No administrator rights required.
;
; Defines are supplied by CI via ISCC command-line flags:
;   /DAppVersion=<version>   e.g. 0.1.12
;   /DSourceExe=<path>       absolute path to the built scode.exe
;   /DOutputDir=<dir>        directory to write the setup .exe into
;   /DOutputBase=<name>      output filename without extension

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef SourceExe
  #define SourceExe "scode.exe"
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif
#ifndef OutputBase
  #define OutputBase "scode-setup"
#endif

[Setup]
AppId={{9C2E7B7A-1F2D-4C3E-9A5B-5C0DE0000001}
AppName=scode
AppVersion={#AppVersion}
AppPublisher=sudocode
DefaultDirName={localappdata}\Programs\scode
DisableProgramGroupPage=yes
UninstallDisplayName=scode
UninstallDisplayIcon={app}\scode.exe
OutputDir={#OutputDir}
OutputBaseFilename={#OutputBase}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
PrivilegesRequired=lowest
ChangesEnvironment=yes

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "scode.exe"; Flags: ignoreversion

[Registry]
; Append the install dir to the per-user PATH (HKA resolves to HKCU for a
; per-user install). Only added when it is not already present.
Root: HKA; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; \
    ValueData: "{olddata};{app}"; \
    Check: NeedsAddPath('{app}')

[Code]
function NeedsAddPath(Param: string): Boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKA, 'Environment', 'Path', OrigPath) then
  begin
    Result := True;
    exit;
  end;
  { Wrap in semicolons so we match whole path segments, not substrings. }
  Result := Pos(';' + Uppercase(Param) + ';', ';' + Uppercase(OrigPath) + ';') = 0;
end;
