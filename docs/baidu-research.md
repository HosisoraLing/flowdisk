# 百度下载实现调研（2026-10-19）

| 项目 | 核对内容 | 对本项目的用途与边界 |
| --- | --- | --- |
| [komorebiCarry/BaiduPCS-Rust](https://github.com/komorebiCarry/BaiduPCS-Rust) | Rust、Web、Docker；`netdisk/client.rs` 的 Locate 下载；README 的 v2.2.2 分段修复及普通账号并发建议 | 最贴近当前技术栈。参考取链协议、单请求上限及按会员等级控制并发；不复制其完整任务系统。项目明确建议普通账号单线程、单任务，不能理解为普通账号稳定不限速。 |
| [qjfoidnh/BaiduPCS-Go](https://github.com/qjfoidnh/BaiduPCS-Go) | `baidupcs/prepare.go` 的 PCS download / locatedownload 与 `baidupcs/download.go` 的多地址结构 | 参考官方原文件地址获取与备用接口。Go 程序没有成为部署依赖。 |
| [acgotaku/BaiduExporter](https://github.com/acgotaku/BaiduExporter) | 导出下载地址给 aria2 / aria2 RPC | 可借鉴导出和分段思想；aria2 接收已有地址，不提供账号权限或速度保证。当前 Rust 已具备 Range 分段、续传和 SHA-256，无需再维护下载进程。 |
| [88250/baidu-netdisk-downloaderx](https://github.com/88250/baidu-netdisk-downloaderx) | 作者仓库说明 | 作者说明已于 2020-04-15 停用，源码仅供交流；不作为当前可用加速服务。 |

## 已做的调整

- 原文件取链优先使用本人 Cookie 的官方 PCS Locate 接口，失败回退 `filemetas`；仅接受官方 HTTPS 下载域名。Locate 使用与取链完全一致的 UA，filemetas 使用相应的下载 UA，不能交叉混用。
- 下载使用经过验证的 UA；百度的每次 HTTP Range 最多 4 MiB。磁盘原有 8 MiB 分段仍能续传，分段内部拆成较小请求，避免为了调整请求大小重下已有数据。
- 自动读取实际会员等级：普通账号 1 连接、VIP 4、SVIP 8，且受全局连接限制。百度同时只运行一个文件，避免多个文件争抢同一账号配额。部署者可以用 `BAIDU_CONNECTIONS=1..16` 覆盖连接数。
- 连接提前结束时，先刷写已收到的分段，再从该字节继续；有明确进展时继续恢复，连续无进展的失败仍有上限。429 / 503 退避、过期链接刷新与内容校验仍保留。

## 实测与限制

同一账号、同一 75.4 MiB 文件的短时取样中，1 / 4 / 8 连接分别约 13.9 / 12.9 / 10.1 KiB/s；测试时还有用户下载在运行，属于有竞争的诊断样本，不能作为独立速度基准。可说明增加线程没有在该次样本中提高速度，不能据此给出固定限速结论。官方 Locate 的备用地址也成功返回 206，但短样本未证明持续提速。

没有发现并验证一套能让当前普通账号持续不限速的独立开源实现。未接入需要把用户 Cookie 交给第三方的解析服务。实际速度以页面实时统计和完整文件校验为准。

## 完整下载验证

修正协议后，同一 75.4 MiB 文件从原先失败的分段恢复并完整落盘；长度 79,020,467 字节，与远端 MD5 相符，独立 SHA-256 与任务记录一致。本任务已记录的峰值约 426.4 KiB/s、平均约 60.3 KiB/s；统计包含之前已经记录的失败尝试和等待取链的活跃时间，排除暂停与排队，不是独立速度基准。这验证了完整文件恢复，未验证普通账号持续不限速。
