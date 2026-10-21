# MCP 连接

服务地址为工作台地址后加 `/mcp`，使用 Streamable HTTP 的 JSON 响应模式，兼容 `2025-03-26`、`2025-06-18` 和 `2025-11-25` 协议协商。未实现 SSE 订阅或服务端主动请求；客户端应使用 HTTP MCP 连接方式。

在 MCP 客户端中设置：

- URL：例如 `http://127.0.0.1:8080/mcp`。
- 请求头：`Authorization: Bearer <部署时的 APP_TOKEN>`。
- 远程连接建议使用 HTTPS。不要把真实令牌写入公开配置、截图、仓库或聊天。

可调用的工具：

| 工具 | 用途 |
| --- | --- |
| `flowdisk_status` | 服务、账号及 SMB 连接状态 |
| `flowdisk_browse` | 分页浏览已授权的网盘或 SMB 目录 |
| `flowdisk_list_downloads` | 统一列出下载和做种任务 |
| `flowdisk_download_link` | 添加普通链接或磁力任务 |
| `flowdisk_parse_share` | 解析分享，等待登录/转存确认/选择 |
| `flowdisk_share_contents` | 分享目录和容量估算 |
| `flowdisk_prepare_share` | 确认转存，必须先向用户展示空间并得到同意 |
| `flowdisk_download_share` | 下载明确选中的分享文件 |
| `flowdisk_download_paths` | 下载网盘或 SMB 已存文件/目录 |
| `flowdisk_select_torrent` | 确认种子文件编号并开始下载 |
| `flowdisk_task_control` | 暂停/继续下载，暂停做种 |
| `flowdisk_statistics` | 读取速率、流量、任务和空间统计 |
| `flowdisk_settings` | 查看或修改总下载限速、做种上传上限和停止策略 |

限速参数按 bytes/s，0 表示不限速；与网页界面的 KiB/s 单位不同。工具调用复用工作台 API 的路径、文件选择、容量、广告及活动任务校验，不开放任意 HTTP 代理、任意文件读写或命令执行。

MCP 令牌拥有当前可信用户的工作台权限。只读工具带 `readOnlyHint`；添加任务和修改设置会产生真实操作，客户端应向用户展示工具调用。分享需先浏览并选择，转存需 `confirm_transfer:true`；不要由模型自行代替用户作出空间消耗或广告保留决定。
