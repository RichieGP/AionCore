# 17 - Shell 与语音

## 概述

提供两类独立功能：(1) Shell 操作——在服务器本地打开文件、目录、URL 以及检测/启动外部工具（VS Code、终端、文件管理器）；(2) 语音转文字——接收前端录音的音频数据，代理转发至第三方 STT 服务（OpenAI Whisper / Deepgram），返回转录文本。

**源码位置**：
- `process/bridge/shellBridge.ts` — Electron 桌面端 Shell 操作
- `process/bridge/shellBridgeStandalone.ts` — Standalone 模式 Shell 操作
- `process/bridge/speechToTextBridge.ts` — 语音转文字 IPC 入口
- `process/bridge/services/SpeechToTextService.ts` — STT 服务核心实现
- `common/types/speech.ts` — 语音相关类型定义
- `renderer/services/SpeechToTextService.ts` — 前端 STT 调用（含 WebUI HTTP 路径）

---

## 子模块划分

| 子模块 | 原始源码 | 迁移策略 |
|--------|---------|---------|
| Shell 操作 | `shellBridge.ts` + `shellBridgeStandalone.ts` | 迁移 — Standalone 实现为主，合并两种实现的功能集 |
| 语音转文字 | `speechToTextBridge.ts` + `SpeechToTextService.ts` | 迁移 — 多 Provider API 代理 |

---

## IPC 接口

### 1. Shell 操作

#### `open-file` — 打开文件

- **目标协议**：HTTP（REST API）
- **参数**：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `filePath` | `string` | 是 | 文件的绝对路径 |

- **返回值**：无（成功时无返回，失败时返回错误）
- **功能语义**：使用操作系统默认程序打开指定文件
  - macOS：`open <filePath>`
  - Linux：`xdg-open <filePath>`
  - Windows：`cmd /c start "" <filePath>`
- **错误场景**：文件不存在、无关联的默认程序、权限不足

#### `show-item-in-folder` — 在文件管理器中显示

- **目标协议**：HTTP（REST API）
- **参数**：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `filePath` | `string` | 是 | 文件的绝对路径 |

- **返回值**：无
- **功能语义**：在系统文件管理器中打开文件所在目录并定位到该文件
  - macOS：`open -R <filePath>`（Reveal in Finder）
  - Linux/Windows：打开父目录
- **错误场景**：路径不存在

#### `open-external` — 打开外部 URL

- **目标协议**：HTTP（REST API）
- **参数**：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `url` | `string` | 是 | 要打开的 URL |

- **返回值**：无
- **功能语义**：使用系统默认浏览器打开指定 URL
- **安全**：Standalone 实现中会验证 URL 格式合法性（必须是有效 URL），防止命令注入
- **错误场景**：URL 格式非法、无默认浏览器

#### `shell.check-tool-installed` — 检测工具是否已安装

- **目标协议**：HTTP（REST API）
- **参数**：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `tool` | `'vscode' \| 'terminal' \| 'explorer'` | 是 | 要检测的工具 |

- **返回值**：`boolean`
- **功能语义**：
  - `vscode`：检测 VS Code 是否安装（通过 `which code` 或平台特定路径）
  - `terminal`：始终返回 `true`（所有桌面系统都有终端）
  - `explorer`：始终返回 `true`（所有桌面系统都有文件管理器）
- **平台特定检测**：
  - macOS：检查 `/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code`
  - Windows：扫描 `%ProgramFiles%` 和 `%LOCALAPPDATA%` 下的 `code.cmd`
  - Linux：通过 `which code` 检测

> **注**：此接口仅存在于 Electron 实现。Standalone 实现未提供此功能。Rust 重写时评估是否需要保留。

#### `shell.open-folder-with` — 使用指定工具打开目录

- **目标协议**：HTTP（REST API）
- **参数**：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `folderPath` | `string` | 是 | 目录的绝对路径 |
| `tool` | `'vscode' \| 'terminal' \| 'explorer'` | 是 | 打开方式 |

- **返回值**：无
- **功能语义**：
  - `vscode`：执行 `code <folderPath>`
  - `terminal`：在目录中打开系统终端
    - macOS：`open -a Terminal <folderPath>`
    - Windows：`cmd /c start cmd /K "cd /d <folderPath>"`
    - Linux：依次尝试 `gnome-terminal`、`konsole`、`xfce4-terminal`、`x-terminal-emulator`、`terminator`
  - `explorer`：在系统文件管理器中打开目录
    - macOS：`open <folderPath>`
    - Windows：`explorer <folderPath>`
    - Linux：`xdg-open <folderPath>`
- **错误场景**：目录不存在、工具未安装（VS Code）

> **注**：此接口仅存在于 Electron 实现。Rust 重写时考虑合并到 Standalone 实现中。

---

### 2. 语音转文字

#### `speech-to-text.transcribe` — 音频转录

- **目标协议**：HTTP（REST API）
- **参数**：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `audioBuffer` | `Uint8Array \| number[] \| Record<string, number>` | 是 | 音频二进制数据 |
| `fileName` | `string` | 是 | 文件名（含扩展名，用于 MIME 推断） |
| `languageHint` | `string?` | 否 | 语言提示（ISO 639-1，如 `'en'`、`'zh'`） |
| `mimeType` | `string` | 是 | 音频 MIME 类型 |

- **返回值**：

| 字段 | 类型 | 说明 |
|------|------|------|
| `text` | `string` | 转录文本 |
| `model` | `string` | 使用的模型名称 |
| `provider` | `'openai' \| 'deepgram'` | 使用的 STT Provider |
| `language` | `string?` | 检测到的语言（仅 Deepgram 返回） |

- **功能语义**：
  1. 从系统设置中读取 STT 配置（Provider 选择、API Key、模型等）
  2. 检查 STT 功能是否启用，未启用则返回 `STT_DISABLED` 错误
  3. 将 `audioBuffer` 归一化为 `Uint8Array`（兼容不同序列化格式）
  4. 根据配置的 Provider 调用对应的转录 API：
     - **OpenAI**：`POST /v1/audio/transcriptions`，multipart/form-data 格式
     - **Deepgram**：`POST /v1/listen`，raw binary 格式
  5. 返回转录结果

- **OpenAI 请求详情**：
  - URL：`{baseUrl}/v1/audio/transcriptions`（默认 `https://api.openai.com`）
  - Headers：`Authorization: Bearer {apiKey}`
  - Body（multipart/form-data）：
    - `file`：音频文件 Blob
    - `model`：模型名称（如 `whisper-1`）
    - `language`：语言提示（可选，来自配置或请求参数）
    - `prompt`：提示词（可选，来自配置）
    - `temperature`：温度参数（可选，来自配置）
  - 响应：`{ text: string }`

- **Deepgram 请求详情**：
  - URL：`{baseUrl}/v1/listen?model={model}&...`（默认 `https://api.deepgram.com`）
  - Query 参数：`model`、`language`（或 `detect_language=true`）、`smart_format`、`punctuate`
  - Headers：`Authorization: Token {apiKey}`、`Content-Type: {mimeType}`
  - Body：raw 音频二进制数据
  - 响应：`{ results.channels[0].alternatives[0].transcript, metadata.model_info[*].name }`

- **错误码**：

| 错误码 | 说明 |
|--------|------|
| `STT_DISABLED` | STT 功能未启用 |
| `STT_OPENAI_NOT_CONFIGURED` | OpenAI Provider 未配置 API Key |
| `STT_DEEPGRAM_NOT_CONFIGURED` | Deepgram Provider 未配置 API Key |
| `STT_REQUEST_FAILED` | 第三方 API 请求失败（含 HTTP 状态码和响应体） |
| `STT_UNKNOWN` | 未知错误 |

---

## REST API

### `POST /api/stt` — 语音转文字（WebUI 模式）

前端 WebUI 模式下（非 Electron IPC），通过此 HTTP 端点调用 STT 服务。功能语义与 `speech-to-text.transcribe` IPC 完全相同。

> **注**：原实现中此路由的具体定义分散在 renderer 侧的条件判断中。Rust 重写时应在后端统一提供此 REST API，IPC 和 HTTP 走同一个服务实现。

### 前端音频处理

前端在调用 STT 前会进行以下预处理（仅供接口设计参考，不迁移）：

| 检查项 | 说明 |
|--------|------|
| 文件大小限制 | 最大 30 MB |
| 扩展名映射 | `audio/mp4`/`audio/m4a` → `.m4a`、`audio/mpeg` → `.mp3`、`audio/ogg`/`audio/opus` → `.ogg`、`audio/wav` → `.wav`、其他 → `.webm` |

---

## 数据模型

### SpeechToTextProvider

```
'openai' | 'deepgram'
```

### SpeechToTextConfig

语音转文字的完整配置，存储在系统设置中。

| 字段 | 类型 | 说明 |
|------|------|------|
| `enabled` | `boolean` | 是否启用 STT 功能 |
| `provider` | `SpeechToTextProvider` | 当前使用的 Provider |
| `autoSend` | `boolean?` | 转录完成后是否自动发送（前端行为） |
| `openai` | `OpenAISpeechToTextConfig?` | OpenAI Provider 配置 |
| `deepgram` | `DeepgramSpeechToTextConfig?` | Deepgram Provider 配置 |

### OpenAISpeechToTextConfig

| 字段 | 类型 | 说明 |
|------|------|------|
| `apiKey` | `string` | API Key |
| `baseUrl` | `string?` | 自定义 Base URL（兼容 OpenAI API 格式的第三方服务） |
| `model` | `string` | 模型名称（如 `whisper-1`） |
| `language` | `string?` | 默认语言（ISO 639-1） |
| `prompt` | `string?` | 提示词（引导转录风格/术语） |
| `temperature` | `number?` | 温度参数 |

### DeepgramSpeechToTextConfig

| 字段 | 类型 | 说明 |
|------|------|------|
| `apiKey` | `string` | API Key |
| `baseUrl` | `string?` | 自定义 Base URL |
| `model` | `string` | 模型名称 |
| `language` | `string?` | 语言代码 |
| `detectLanguage` | `boolean?` | 是否自动检测语言 |
| `punctuate` | `boolean?` | 是否添加标点 |
| `smartFormat` | `boolean?` | 是否启用智能格式化 |

### SpeechToTextRequest

| 字段 | 类型 | 说明 |
|------|------|------|
| `audioBuffer` | `binary` | 音频二进制数据 |
| `fileName` | `string` | 文件名 |
| `languageHint` | `string?` | 语言提示 |
| `mimeType` | `string` | MIME 类型 |

> **设计决策**：原实现中 `audioBuffer` 接受三种格式（`Uint8Array | number[] | Record<string, number>`）以兼容不同 IPC 序列化方式。Rust 重写时 REST API 直接接收 binary body 或 multipart/form-data，无需此类兼容处理。

### SpeechToTextResult

| 字段 | 类型 | 说明 |
|------|------|------|
| `text` | `string` | 转录文本 |
| `model` | `string` | 使用的模型 |
| `provider` | `SpeechToTextProvider` | 使用的 Provider |
| `language` | `string?` | 检测到的语言 |

### ToolType（Shell）

```
'vscode' | 'terminal' | 'explorer'
```

---

## 模块依赖

### 依赖的模块

| 模块 | 依赖内容 |
|------|---------|
| 系统设置（04） | 读取 STT 配置（Provider 选择、API Key、模型参数等） |

### 被依赖的模块

无其他模块直接依赖本模块。

---

## 候选公共类型

| 类型 | 说明 | 归属建议 |
|------|------|---------|
| `SpeechToTextProvider` | STT Provider 枚举 | `aionui-api-types` |
| `SpeechToTextConfig` | STT 完整配置 | `aionui-api-types`（与系统设置共享） |
| `SpeechToTextRequest` | STT 请求参数 | `aionui-api-types` |
| `SpeechToTextResult` | STT 转录结果 | `aionui-api-types` |

---

## 设计决策

### 1. Shell 操作的两种实现合并

原实现中 Shell 操作分为 Electron 版（使用 `electron.shell` API）和 Standalone 版（使用 `child_process.execFile`）。功能集不完全一致：

| 接口 | Electron 版 | Standalone 版 |
|------|------------|--------------|
| `openFile` | ✅ `shell.openPath` | ✅ `execFile('open')` |
| `showItemInFolder` | ✅ `shell.showItemInFolder` | ✅ `execFile('open', ['-R'])` / 打开父目录 |
| `openExternal` | ✅ `shell.openExternal` | ✅ `execFile('open')` + URL 验证 |
| `checkToolInstalled` | ✅ | ❌ |
| `openFolderWith` | ✅ | ❌ |

**改进方向**：Rust 重写时统一为一个实现，涵盖所有功能。使用 `std::process::Command` 实现跨平台命令执行。`checkToolInstalled` 和 `openFolderWith` 作为 REST API 统一提供。

### 2. Shell 操作的安全考量

Shell 操作允许在服务器端执行系统命令（打开文件、启动程序），存在命令注入风险。

**改进方向**：
- 所有文件/目录路径必须经过规范化和存在性校验
- URL 参数必须通过严格的 URL 解析验证（不能包含命令注入字符）
- 考虑白名单机制：限制可打开的文件类型或目录范围
- 日志记录所有 Shell 操作，便于审计
- 如果 Rust 后端部署在远程服务器（非本地），应考虑是否暴露这些操作（可能需要权限控制或完全禁用）

### 3. STT 音频传输方式优化

原实现中通过 IPC 传输 audioBuffer（JSON 序列化的二进制数据），效率较低。WebUI 模式已使用 HTTP POST。

**改进方向**：Rust 重写时统一使用 HTTP `multipart/form-data` 传输音频文件，避免 JSON 序列化二进制数据的开销。请求格式：

```
POST /api/stt
Content-Type: multipart/form-data

- file: <audio binary>
- fileName: <string>
- mimeType: <string>
- languageHint: <string> (optional)
```

### 4. STT Provider 扩展性

原实现硬编码了 OpenAI 和 Deepgram 两个 Provider。

**改进方向**：Rust 重写时采用 trait 抽象 Provider 接口，便于未来添加新 Provider（如 Azure Speech、Google STT）。Provider 配置以 JSON 对象形式存储在系统设置中，新增 Provider 仅需实现 trait 和添加配置项，无需修改核心逻辑。
