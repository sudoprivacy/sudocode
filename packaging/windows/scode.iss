; Inno Setup script for the `scode` CLI.
;
; Builds a per-user installer that drops scode.exe into a directory and
; appends that directory to the user's PATH, so `scode` works from any
; new terminal after install. No administrator rights required.
;
; It also runs a first-run configuration wizard: the user enters their
; sudorouter API key, the installer fetches the available model list over
; HTTPS, and writes ready-to-use sudocode.json + settings.json into the
; per-user config home (~/.nexus/sudocode) so `scode` works with no extra
; manual setup. The wizard is skipped when a sudocode.json already exists.
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

[Tasks]
; Default-checked: append the install dir to the user's PATH.
Name: "addtopath"; Description: "将安装目录加入 PATH（推荐，新开终端即可运行 scode）"

[Files]
Source: "{#SourceExe}"; DestDir: "{app}"; DestName: "scode.exe"; Flags: ignoreversion

[Registry]
; Append the install dir to the per-user PATH (HKA resolves to HKCU for a
; per-user install). Only added when the task is selected and it is not
; already present.
Root: HKA; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; \
    ValueData: "{olddata};{app}"; \
    Check: ShouldAddPath('{app}')

[Code]
const
  DefaultBaseUrl = 'https://hk.sudorouter.ai/v1';
  DefaultModel = 'deepseek-v4-pro';

var
  ConfigPage: TWizardPage;
  BaseUrlEdit: TNewEdit;
  ApiKeyEdit: TPasswordEdit;
  FetchButton: TNewButton;
  PresetButton: TNewButton;
  ModelCombo: TNewComboBox;
  SearchCheck: TNewCheckBox;
  StatusLabel: TNewStaticText;
  ConfigDone: Boolean;

{ ---- PATH helper ------------------------------------------------------- }

function ShouldAddPath(Param: string): Boolean;
var
  OrigPath: string;
  AlreadyPresent: Boolean;
begin
  if not WizardIsTaskSelected('addtopath') then
  begin
    Result := False;
    exit;
  end;
  if not RegQueryStringValue(HKA, 'Environment', 'Path', OrigPath) then
  begin
    Result := True;
    exit;
  end;
  { Wrap in semicolons so we match whole path segments, not substrings. }
  AlreadyPresent := Pos(';' + Uppercase(Param) + ';', ';' + Uppercase(OrigPath) + ';') > 0;
  Result := not AlreadyPresent;
end;

{ ---- Small string / JSON helpers -------------------------------------- }

function TrimTrailingSlashes(S: string): string;
begin
  while (Length(S) > 0) and (S[Length(S)] = '/') do
    Delete(S, Length(S), 1);
  Result := S;
end;

{ Minimal JSON string escaping (backslash + double quote), wrapped in quotes. }
function JsonStr(S: string): string;
var
  I: Integer;
  C: Char;
  R: string;
begin
  R := '';
  for I := 1 to Length(S) do
  begin
    C := S[I];
    if C = '\' then
      R := R + '\\'
    else if C = '"' then
      R := R + '\"'
    else
      R := R + C;
  end;
  Result := '"' + R + '"';
end;

{ Name-based inference of image-input support, mirroring config-tool.html. }
function IsVisionModel(Id: string): Boolean;
var
  S: string;
begin
  S := Lowercase(Id);
  Result :=
    (Pos('gpt-5', S) > 0) or (Pos('gpt-4o', S) > 0) or (Pos('gpt-4.1', S) > 0) or
    (Pos('gemini', S) > 0) or (Pos('claude-3', S) > 0) or (Pos('claude-opus', S) > 0) or
    (Pos('claude-sonnet', S) > 0) or (Pos('claude-haiku', S) > 0) or
    (Pos('vision', S) > 0) or (Pos('-vl', S) > 0) or (Pos('llava', S) > 0) or
    (Pos('pixtral', S) > 0) or (Pos('-image', S) > 0) or (Pos('multimodal', S) > 0) or
    (Pos('omni', S) > 0);
end;

{ ---- Config directory ------------------------------------------------- }

function ConfigDir: string;
begin
  { scode's default config home when SUDO_CODE_CONFIG_HOME is unset. }
  Result := ExpandConstant('{%USERPROFILE}') + '\.nexus\sudocode';
end;

function ConfigAlreadyExists: Boolean;
begin
  Result := FileExists(ConfigDir + '\sudocode.json');
end;

{ ---- Model list fetching ---------------------------------------------- }

{ GET {baseUrl}/models with a bearer token via WinHTTP. Returns True on
  HTTP 200 with the body in Body; otherwise Body carries an error message. }
function HttpGetModels(BaseUrl, ApiKey: string; var Body: string): Boolean;
var
  WinHttp: Variant;
begin
  Result := False;
  try
    WinHttp := CreateOleObject('WinHttp.WinHttpRequest.5.1');
    WinHttp.Open('GET', BaseUrl + '/models', False);
    WinHttp.SetRequestHeader('Authorization', 'Bearer ' + ApiKey);
    WinHttp.Send('');
    if WinHttp.Status = 200 then
    begin
      Body := WinHttp.ResponseText;
      Result := True;
    end
    else
      Body := 'HTTP ' + IntToStr(WinHttp.Status);
  except
    Body := GetExceptionMessage;
  end;
end;

{ Scan an OpenAI-style /models response for each "id":"..." value. }
procedure ParseModelIds(Json: string; List: TStringList);
var
  Rest, Id: string;
  P, Q: Integer;
begin
  List.Clear;
  Rest := Json;
  repeat
    P := Pos('"id"', Rest);
    if P = 0 then
      Break;
    Rest := Copy(Rest, P + 4, Length(Rest));
    P := Pos('"', Rest);
    if P = 0 then
      Break;
    Rest := Copy(Rest, P + 1, Length(Rest));
    Q := Pos('"', Rest);
    if Q = 0 then
      Break;
    Id := Copy(Rest, 1, Q - 1);
    Rest := Copy(Rest, Q + 1, Length(Rest));
    if (Id <> '') and (List.IndexOf(Id) < 0) then
      List.Add(Id);
  until False;
end;

{ Built-in fallback list mirroring config-tool.html's PRESET_MODELS. }
procedure LoadPresetModels(List: TStringList);
begin
  List.Clear;
  List.Add('auto');
  List.Add('grok-4-20-reasoning');
  List.Add('grok-4.3');
  List.Add('deepseek-v4-flash');
  List.Add('deepseek-v4-pro');
  List.Add('glm-5.1');
  List.Add('MiniMax-M2.5');
  List.Add('gemini-3.5-flash');
  List.Add('gemini-3.1-flash-lite');
  List.Add('gemini-3-flash-preview');
  List.Add('gemini-3.1-pro-preview');
  List.Add('Kimi-K2.6');
  List.Add('gpt-5.3-codex');
  List.Add('gpt-5.4');
  List.Add('gpt-5.5');
  List.Add('claude-sonnet-4-6');
  List.Add('claude-opus-4-6');
  List.Add('claude-opus-4-7');
  List.Add('claude-opus-4-8');
end;

{ Fill the dropdown from a list of model ids; preselect DefaultModel. }
procedure PopulateModelCombo(List: TStringList);
var
  I, DefIdx: Integer;
begin
  ModelCombo.Items.Clear;
  DefIdx := -1;
  for I := 0 to List.Count - 1 do
  begin
    ModelCombo.Items.Add(List[I]);
    if List[I] = DefaultModel then
      DefIdx := I;
  end;
  if ModelCombo.Items.Count > 0 then
  begin
    if DefIdx >= 0 then
      ModelCombo.ItemIndex := DefIdx
    else
      ModelCombo.ItemIndex := 0;
  end;
end;

procedure FetchButtonClick(Sender: TObject);
var
  Body, BaseUrl, ApiKey: string;
  List: TStringList;
begin
  BaseUrl := TrimTrailingSlashes(Trim(BaseUrlEdit.Text));
  ApiKey := Trim(ApiKeyEdit.Text);
  if (BaseUrl = '') or ((Pos('http://', Lowercase(BaseUrl)) <> 1) and (Pos('https://', Lowercase(BaseUrl)) <> 1)) then
  begin
    StatusLabel.Caption := '请先填写正确的 Base URL（需以 http:// 或 https:// 开头）';
    exit;
  end;
  if ApiKey = '' then
  begin
    StatusLabel.Caption := '请先填写 API Key';
    exit;
  end;
  StatusLabel.Caption := '正在拉取模型列表…';
  List := TStringList.Create;
  try
    if HttpGetModels(BaseUrl, ApiKey, Body) then
    begin
      ParseModelIds(Body, List);
      if List.Count = 0 then
        StatusLabel.Caption := '接口返回的模型列表为空，可点「使用内置预设」'
      else
      begin
        PopulateModelCombo(List);
        StatusLabel.Caption := '✓ 已拉取 ' + IntToStr(List.Count) + ' 个模型';
      end;
    end
    else
      StatusLabel.Caption := '拉取失败：' + Body + ' —— 可点「使用内置预设」';
  finally
    List.Free;
  end;
end;

procedure PresetButtonClick(Sender: TObject);
var
  List: TStringList;
begin
  List := TStringList.Create;
  try
    LoadPresetModels(List);
    PopulateModelCombo(List);
    StatusLabel.Caption := '已加载内置预设模型列表（' + IntToStr(List.Count) + ' 个）';
  finally
    List.Free;
  end;
end;

{ ---- Config file generation ------------------------------------------- }

function BuildSudocodeJson(BaseUrl, ApiKey, ModelsBlock: string; EnableSearch: Boolean): string;
begin
  Result :=
    '{' + #13#10 +
    '  "models": {' + #13#10 +
    ModelsBlock + #13#10 +
    '  },' + #13#10 +
    '  "auth_modes": {' + #13#10 +
    '    "proxy": {' + #13#10 +
    '      "sudorouter": { "baseUrl": ' + JsonStr(BaseUrl) + ', "apiKey": ' + JsonStr(ApiKey) + ' }' + #13#10 +
    '    }' + #13#10 +
    '  }';
  if EnableSearch then
    Result := Result + ',' + #13#10 +
      '  "web_search": {' + #13#10 +
      '    "provider": "tavily",' + #13#10 +
      '    "apiUrl": "https://hk.sudorouter.ai/search/tavily/search",' + #13#10 +
      '    "apiKey": ""' + #13#10 +
      '  }';
  Result := Result + #13#10 + '}' + #13#10;
end;

function BuildModelsBlock: string;
var
  I: Integer;
  Id, InputArr, Block: string;
begin
  Block := '';
  for I := 0 to ModelCombo.Items.Count - 1 do
  begin
    Id := ModelCombo.Items[I];
    if IsVisionModel(Id) then
      InputArr := '["text", "image"]'
    else
      InputArr := '["text"]';
    if I > 0 then
      Block := Block + ',' + #13#10;
    Block := Block +
      '    ' + JsonStr(Id) + ': {' + #13#10 +
      '      "alias": ' + JsonStr(Id) + ',' + #13#10 +
      '      "name": ' + JsonStr(Id) + ',' + #13#10 +
      '      "input": ' + InputArr + ',' + #13#10 +
      '      "providers": {' + #13#10 +
      '        "proxy": { "provider": "sudorouter", "model": ' + JsonStr(Id) + ', "api": "openai-completions" }' + #13#10 +
      '      }' + #13#10 +
      '    }';
  end;
  Result := Block;
end;

procedure WriteConfigFiles;
var
  Dir, BaseUrl, ApiKey, Model, SudoJson, SettingsJson: string;
begin
  if ModelCombo.Items.Count = 0 then
    exit;
  BaseUrl := TrimTrailingSlashes(Trim(BaseUrlEdit.Text));
  ApiKey := Trim(ApiKeyEdit.Text);
  if ModelCombo.ItemIndex >= 0 then
    Model := ModelCombo.Items[ModelCombo.ItemIndex]
  else
    Model := DefaultModel;

  Dir := ConfigDir;
  if not ForceDirectories(Dir) then
  begin
    MsgBox('无法创建配置目录：' + Dir, mbError, MB_OK);
    exit;
  end;

  SudoJson := BuildSudocodeJson(BaseUrl, ApiKey, BuildModelsBlock, SearchCheck.Checked);
  SettingsJson := '{ "model": ' + JsonStr(Model) + ' }' + #13#10;

  if not SaveStringToFile(Dir + '\sudocode.json', SudoJson, False) then
    MsgBox('写入 sudocode.json 失败：' + Dir, mbError, MB_OK);
  if not SaveStringToFile(Dir + '\settings.json', SettingsJson, False) then
    MsgBox('写入 settings.json 失败：' + Dir, mbError, MB_OK);
end;

{ ---- Wizard page wiring ----------------------------------------------- }

procedure CreateConfigPage;
var
  Y: Integer;
  Hint: TNewStaticText;
begin
  ConfigPage := CreateCustomPage(wpSelectDir, '配置 scode',
    '填写 API Key 并拉取模型，安装时将自动生成配置文件到 ~/.nexus/sudocode');

  Y := ScaleY(8);

  { Base URL }
  Hint := TNewStaticText.Create(WizardForm);
  Hint.Parent := ConfigPage.Surface;
  Hint.Top := Y;
  Hint.Caption := 'Base URL（API 服务地址，通常以 /v1 结尾）';
  Y := Y + Hint.Height + ScaleY(2);

  BaseUrlEdit := TNewEdit.Create(WizardForm);
  BaseUrlEdit.Parent := ConfigPage.Surface;
  BaseUrlEdit.Top := Y;
  BaseUrlEdit.Width := ConfigPage.SurfaceWidth;
  BaseUrlEdit.Text := DefaultBaseUrl;
  Y := Y + BaseUrlEdit.Height + ScaleY(12);

  { API Key }
  Hint := TNewStaticText.Create(WizardForm);
  Hint.Parent := ConfigPage.Surface;
  Hint.Top := Y;
  Hint.Caption := 'API Key（你的密钥，一般以 sk- 开头）';
  Y := Y + Hint.Height + ScaleY(2);

  ApiKeyEdit := TPasswordEdit.Create(WizardForm);
  ApiKeyEdit.Parent := ConfigPage.Surface;
  ApiKeyEdit.Top := Y;
  ApiKeyEdit.Width := ConfigPage.SurfaceWidth;
  Y := Y + ApiKeyEdit.Height + ScaleY(12);

  { Fetch / preset buttons }
  FetchButton := TNewButton.Create(WizardForm);
  FetchButton.Parent := ConfigPage.Surface;
  FetchButton.Top := Y;
  FetchButton.Width := ScaleX(110);
  FetchButton.Height := ScaleY(25);
  FetchButton.Caption := '拉取模型';
  FetchButton.OnClick := @FetchButtonClick;

  PresetButton := TNewButton.Create(WizardForm);
  PresetButton.Parent := ConfigPage.Surface;
  PresetButton.Top := Y;
  PresetButton.Left := FetchButton.Left + FetchButton.Width + ScaleX(10);
  PresetButton.Width := ScaleX(130);
  PresetButton.Height := ScaleY(25);
  PresetButton.Caption := '使用内置预设';
  PresetButton.OnClick := @PresetButtonClick;
  Y := Y + FetchButton.Height + ScaleY(12);

  { Default model dropdown }
  Hint := TNewStaticText.Create(WizardForm);
  Hint.Parent := ConfigPage.Surface;
  Hint.Top := Y;
  Hint.Caption := '默认模型（拉取或加载预设后从下拉框选择）';
  Y := Y + Hint.Height + ScaleY(2);

  ModelCombo := TNewComboBox.Create(WizardForm);
  ModelCombo.Parent := ConfigPage.Surface;
  ModelCombo.Top := Y;
  ModelCombo.Width := ConfigPage.SurfaceWidth;
  ModelCombo.Style := csDropDownList;
  Y := Y + ModelCombo.Height + ScaleY(12);

  { web_search }
  SearchCheck := TNewCheckBox.Create(WizardForm);
  SearchCheck.Parent := ConfigPage.Surface;
  SearchCheck.Top := Y;
  SearchCheck.Width := ConfigPage.SurfaceWidth;
  SearchCheck.Caption := '启用联网搜索 web_search（密钥自动复用上面的 API Key）';
  SearchCheck.Checked := True;
  Y := Y + SearchCheck.Height + ScaleY(12);

  { Status line }
  StatusLabel := TNewStaticText.Create(WizardForm);
  StatusLabel.Parent := ConfigPage.Surface;
  StatusLabel.Top := Y;
  StatusLabel.Width := ConfigPage.SurfaceWidth;
  StatusLabel.AutoSize := False;
  StatusLabel.Height := ScaleY(34);
  StatusLabel.WordWrap := True;
  StatusLabel.Caption := '';
end;

procedure InitializeWizard;
begin
  ConfigDone := False;
  CreateConfigPage;
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := False;
  { Skip the wizard entirely if the user already has a sudocode.json. }
  if (PageID = ConfigPage.ID) and ConfigAlreadyExists then
    Result := True;
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := True;
  if CurPageID = ConfigPage.ID then
  begin
    if Trim(ApiKeyEdit.Text) = '' then
    begin
      MsgBox('请填写 API Key。', mbError, MB_OK);
      Result := False;
      exit;
    end;
    if ModelCombo.Items.Count = 0 then
    begin
      MsgBox('请先点击「拉取模型」或「使用内置预设」，再选择默认模型。', mbError, MB_OK);
      Result := False;
      exit;
    end;
    ConfigDone := True;
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if (CurStep = ssPostInstall) and ConfigDone then
    WriteConfigFiles;
end;
