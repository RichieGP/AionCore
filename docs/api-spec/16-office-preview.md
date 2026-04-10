# 16 - Office 文档预览

## 概述

管理 Office 文档（Word、Excel、PPT）的实时预览：通过 `officecli watch` 子进程将文档转换为 HTML 并在本地端口提供服务，后端负责子进程生命周期管理、端口分配、反向代理、工具自动安装/更新；同时管理预览内容的快照历史（版本回溯）；以及自动探测本地运行的 Star Office 服务。

**源码位置**：
- `process/bridge/officeWatchBridge.ts` — Word & Excel 预览
- `process/bridge/pptPreviewBridge.ts` — PPT 预览
- `process/bridge/previewHistoryBridge.ts` — 预览快照历史
- `process/bridge/starOfficeBridge.ts` — Star Office 服务探测
- `process/services/previewHistoryService.ts` — 快照历史存储服务
- `process/webserver/routes/apiRoutes.ts` — Office 预览反向代理路由
- `common/types/preview.ts` — 预览相关类型定义

---

## 子模块划分

| 子模块 | 原始源码 | 迁移策略 |
|--------|---------|---------|
| Word/Excel 预览 | `officeWatchBridge.ts` | 迁移 — 子进程管理、端口分配、会话复用 |
| PPT 预览 | `pptPreviewBridge.ts` | 迁移 — 同上，外加 officecli 自动更新检查 |
| 预览反向代理 | `apiRoutes.ts`（proxy 部分） | 迁移 — SSRF 防护、HTML 注入导航守卫、Location 重写 |
| 预览快照历史 | `previewHistoryBridge.ts` + `previewHistoryService.ts` | 迁移 — 文件系统快照存储 |
| Star Office 探测 | `starOfficeBridge.ts` | 迁移 — 端口扫描、健康检查、结果缓存 |
| officecli 安装/更新 | `officeWatchBridge.ts` + `pptPreviewBridge.ts` | 迁移 — 自动安装脚本、每日更新检查 |

---

## IPC 接口

### 1. Word 预览

#### `word-preview.start` — 启动 Word 文档预览

- **目标协议**：HTTP（REST API）
- **参数**：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `filePath` | `string` | 是 | Word 文档的绝对路径 |

- **返回值**：

| 字段 | 类型 | 说明 |
|------|------|------|
| `url` | `string` | 预览服务的代理 URL（成功时非空，失败时为空字符串） |
| `error` | `string?` | 错误信息（仅失败时存在） |

- **功能语义**：
  1. 解析文件路径的真实路径（resolve symlinks）
  2. 若该文件已有活跃的 watch 会话，直接复用并返回 URL
  3. 否则分配空闲 TCP 端口，启动 `officecli watch <filePath> --port <port>` 子进程
  4. 轮询端口就绪（最多 150 次 × 100ms = 15 秒超时）
  5. 若 `officecli` 未安装（`ENOENT`），自动安装后重试一次
  6. 返回代理 URL 供前端 WebView/iframe 加载
- **状态推送**：通过 `word-preview.status` 向前端推送进度（`starting` → `installing`（可选）→ `ready` / `error`）
- **错误场景**：officecli 未安装且自动安装失败、watch 进程异常退出、端口就绪超时

#### `word-preview.stop` — 停止 Word 文档预览

- **目标协议**：HTTP（REST API）
- **参数**：`{ filePath: string }`
- **返回值**：无
- **功能语义**：延迟 500ms 后终止该文件的 watch 子进程并释放端口。延迟是为了支持前端框架 Strict Mode 的组件双挂载（卸载后立即重新挂载时可复用会话）

#### `word-preview.status` — Word 预览状态推送

- **目标协议**：WebSocket（服务端推送事件）
- **推送载荷**：

| 字段 | 类型 | 说明 |
|------|------|------|
| `state` | `'starting' \| 'installing' \| 'ready' \| 'error'` | 当前状态 |
| `message` | `string?` | 附加信息（错误详情等） |

---

### 2. Excel 预览

#### `excel-preview.start` / `excel-preview.stop` / `excel-preview.status`

与 Word 预览完全相同的接口签名和行为语义，仅文档类型不同。Word 和 Excel 使用独立的会话池（互不影响）。

---

### 3. PPT 预览

#### `ppt-preview.start` — 启动 PPT 文档预览

- **目标协议**：HTTP（REST API）
- **参数**：`{ filePath: string }`
- **返回值**：`{ url: string; error?: string }`
- **功能语义**：与 Word/Excel 预览基本相同。差异点：
  - PPT 使用独立的会话池
  - 通过解析 stdout 中的 `Watch:` 关键字检测就绪（而非纯端口轮询）
  - 首次启动后在后台（5 秒延迟）执行 officecli 版本检查，每 24 小时最多一次，发现新版本时自动更新
- **状态推送**：通过 `ppt-preview.status` 推送

#### `ppt-preview.stop` — 停止 PPT 文档预览

与 Word/Excel 相同：延迟 500ms 后终止。

#### `ppt-preview.status` — PPT 预览状态推送

与 Word/Excel 相同的载荷格式。

---

### 4. 预览快照历史

#### `preview-history.list` — 列出快照列表

- **目标协议**：HTTP（REST API）
- **参数**：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `target` | `PreviewHistoryTarget` | 是 | 标识预览内容的上下文信息 |

- **返回值**：`PreviewSnapshotInfo[]`
- **功能语义**：根据 target 计算 SHA-1 摘要作为目录标识，读取该目录下的 `index.json`，返回所有快照的元数据列表

#### `preview-history.save` — 保存快照

- **目标协议**：HTTP（REST API）
- **参数**：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `target` | `PreviewHistoryTarget` | 是 | 标识预览内容的上下文信息 |
| `content` | `string` | 是 | 快照内容（Markdown、HTML、代码等） |

- **返回值**：`PreviewSnapshotInfo`（新创建的快照元数据）
- **功能语义**：
  1. 在 target 对应目录下创建快照文件（`<timestamp>-<random>.md`）
  2. 更新 `index.json` 索引
  3. 若快照数量超过上限（50），删除最旧的快照
  4. 返回新快照的元数据

#### `preview-history.get-content` — 获取快照内容

- **目标协议**：HTTP（REST API）
- **参数**：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `target` | `PreviewHistoryTarget` | 是 | 标识预览内容的上下文信息 |
| `snapshotId` | `string` | 是 | 快照 ID |

- **返回值**：`{ snapshot: PreviewSnapshotInfo; content: string } | null`
- **功能语义**：根据 target 定位目录，按 snapshotId 查找并读取快照文件内容。找不到时返回 null

---

### 5. Star Office 服务探测

#### `star-office.detect-url` — 探测本地 Star Office 服务

- **目标协议**：HTTP（REST API）
- **参数**：

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `preferredUrl` | `string?` | 否 | 用户偏好的 URL（优先探测其端口） |
| `force` | `boolean?` | 否 | 是否强制忽略缓存（默认 false） |
| `timeoutMs` | `number?` | 否 | 单次健康检查超时（默认 1000ms） |

- **返回值**：`{ success: boolean; data?: { url: string | null }; msg?: string }`
- **功能语义**：
  1. 检查缓存：命中缓存且未过期（命中 TTL 20s / 未命中 TTL 1.5s）则直接返回
  2. 构建候选端口列表：已知端口（preferredUrl 端口、19000、18791）± 24 的扫描半径
  3. 并发探测（最多 6 个并发 worker）：依次对候选 URL 进行健康检查
  4. 健康检查逻辑：`GET /health`（必须 200）→ `GET /status`（检查是否包含 Star Office 状态标记）→ `GET /`（检查页面特征关键字）
  5. 找到第一个健康的 URL 后立即返回，更新缓存
- **状态标记**：`idle`、`writing`、`researching`、`executing`、`syncing`、`error`
- **页面特征关键字**：`star office`、`decorate room`、`asset sidebar`
- **排除关键字**：`openclaw control`（排除误识别）

---

## REST API（反向代理路由）

### `GET /api/ppt-proxy/:port/*` — PPT 预览代理

- **功能**：将请求反向代理到 `http://localhost:<port>/<path>`
- **安全**：端口必须是当前活跃的 PPT 预览会话端口（通过 `isActivePreviewPort()` 验证），拒绝非活跃端口（SSRF 防护）
- **请求处理**：
  - 剥离 hop-by-hop 头和认证头
  - HTML 响应：在 `<head>` 后注入导航守卫脚本（防止 iframe 跳出代理路径）
  - 重定向响应：将 `Location` 头中的 `http://localhost:PORT/...` 重写为 `/api/ppt-proxy/PORT/...`

### `GET /api/office-watch-proxy/:port/*` — Word/Excel 预览代理

- 与 PPT 代理完全相同的行为，端口验证使用 `isActiveOfficeWatchPort()`

---

## 数据模型

### PreviewContentType

```
'markdown' | 'diff' | 'code' | 'html' | 'pdf'
| 'ppt' | 'word' | 'excel' | 'image' | 'url'
```

### PreviewHistoryTarget

标识一个预览内容的上下文，用于计算存储目录的 SHA-1 摘要。

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `contentType` | `PreviewContentType` | 是 | 内容类型 |
| `filePath` | `string?` | 否 | 源文件路径 |
| `workspace` | `string?` | 否 | 工作区标识 |
| `fileName` | `string?` | 否 | 文件名 |
| `title` | `string?` | 否 | 标题 |
| `language` | `string?` | 否 | 编程语言（代码预览时） |
| `conversationId` | `string?` | 否 | 关联的会话 ID |

### PreviewSnapshotInfo

| 字段 | 类型 | 说明 |
|------|------|------|
| `id` | `string` | 快照 ID（文件名去除扩展名） |
| `label` | `string` | 显示标签 |
| `createdAt` | `number` | 创建时间戳（ms） |
| `size` | `number` | 内容大小（bytes） |
| `contentType` | `PreviewContentType` | 内容类型 |
| `fileName` | `string?` | 源文件名 |
| `filePath` | `string?` | 源文件路径 |

### WatchSession（内部状态）

| 字段 | 类型 | 说明 |
|------|------|------|
| `port` | `number` | 分配的本地端口 |
| `aborted` | `boolean` | 会话是否已被中止 |
| `processAlive` | `boolean` | 子进程是否存活 |

---

## 模块依赖

### 依赖的模块

| 模块 | 依赖内容 |
|------|---------|
| 应用生命周期（14） | 应用关闭时调用 `stopAllWatchSessions()` / `stopAllOfficeWatchSessions()` 终止所有子进程 |
| 系统设置（04） | 获取数据目录路径（快照历史存储、officecli 更新检查标记） |

### 被依赖的模块

无其他模块直接依赖本模块。

---

## 候选公共类型

| 类型 | 说明 | 归属建议 |
|------|------|---------|
| `PreviewContentType` | 预览内容类型枚举 | `aionui-common` 或 `aionui-api-types` |
| `PreviewHistoryTarget` | 预览历史定位信息 | `aionui-api-types` |
| `PreviewSnapshotInfo` | 快照元数据 | `aionui-api-types` |

---

## 设计决策

### 1. officecli 子进程管理统一化

原实现中 Word/Excel（`officeWatchBridge`）和 PPT（`pptPreviewBridge`）的子进程管理逻辑高度重复（端口分配、会话复用、延迟停止、自动安装、进程监控），仅在就绪检测方式上有细微差异（端口轮询 vs stdout 解析）。

**改进方向**：Rust 重写时应统一为一个通用的 OfficecliWatchManager，通过文档类型参数区分行为，消除代码重复。就绪检测统一使用端口轮询（更可靠，不依赖 stdout 缓冲行为）。

### 2. 预览代理安全加固

原实现的反向代理已有 SSRF 防护（端口白名单验证）、hop-by-hop 头剥离、HTML 导航守卫注入。Rust 重写时应保留这些安全措施，可考虑：
- 代理目标严格限制为 `127.0.0.1`（不允许其他地址）
- 增加请求频率限制
- HTML 注入使用更健壮的解析方式（而非字符串查找 `<head>`）

### 3. 快照历史存储方式

原实现使用文件系统存储（SHA-1 目录 + index.json + 独立文件），每个 target 最多 50 个快照。这种方案简单且适合本地应用场景。

**保留方向**：Rust 重写中保持文件系统存储策略，无需引入数据库。SHA-1 目录命名方案有效避免了路径长度和特殊字符问题。

### 4. Star Office 探测策略

端口扫描范围（± 24 端口）和并发度（6 workers）是为本地服务场景设计的合理参数。缓存 TTL（命中 20s / 未命中 1.5s）在实际使用中表现良好。

**保留方向**：Rust 重写中保持相同策略，使用 tokio 异步并发探测。
