# 开源实现研究与归属

研究日期：2026-10-16。实际集成通过独立进程的 HTTP API，不复制、链接或修改 OpenList 的驱动代码。

## 采用：OpenList

- 项目：[OpenListTeam/OpenList](https://github.com/OpenListTeam/OpenList)
- 部署版本：[v4.2.6](https://github.com/OpenListTeam/OpenList/releases/tag/v4.2.6)
- 对应提交：`2bdf16d5967d0a403f67d809efd5a418b8f5bd30`
- 镜像：`openlistteam/openlist:v4.2.6`
- 本次拉取的镜像清单摘要：`sha256:c555c6e1c8af2aead38ed12ec761ac077fdf046d19cf033414be8e056aec6b64`
- 许可证：[AGPL-3.0](https://github.com/OpenListTeam/OpenList/blob/v4.2.6/LICENSE)
- 用途：网盘登录、令牌刷新、已存文件目录、原文件下载地址及所需 HTTP 请求头。

核对了以下官方实现与文档：

- [文件列表与元数据接口](https://github.com/OpenListTeam/OpenList/blob/v4.2.6/server/handles/fsread.go)
- [下载链接接口](https://github.com/OpenListTeam/OpenList/blob/v4.2.6/server/handles/fsmanage.go)
- [链接结构和请求头](https://github.com/OpenListTeam/OpenList/blob/v4.2.6/internal/model/args.go)
- [百度驱动](https://github.com/OpenListTeam/OpenList/tree/v4.2.6/drivers/baidu_netdisk) / [文档](https://doc.oplist.org/guide/drivers/baidu)
- [夸克驱动](https://github.com/OpenListTeam/OpenList/tree/v4.2.6/drivers/quark_uc) / [文档](https://doc.oplist.org/guide/drivers/quark)
- [115 Open 驱动](https://github.com/OpenListTeam/OpenList/tree/v4.2.6/drivers/115_open) / [文档](https://doc.oplist.org/guide/drivers/115_open)
- [迅雷驱动](https://github.com/OpenListTeam/OpenList/tree/v4.2.6/drivers/thunder) / [文档](https://doc.oplist.org/guide/drivers/thunder)

选择它是因为四家网盘的适配已集中维护，Cookie、OAuth、风控、签名、UA 绑定和直链过期行为各不相同。独立网关能够更新驱动而不重写 Rust 下载核心。账号可用性仍取决于上游接口与对应驱动的维护情况；旧版逆向驱动不等同于稳定官方 API。

部署中 OpenList 的完整管理界面只负责初始化，FlowDisk v0.2 通过经过参数验证的接口提供创建目录、重命名、移动、删除，以及夸克分享转存；v0.3 增加拒绝覆盖的文件上传。

## Rust 依赖

Rust 下载实现为本项目原创，依赖以下项目；具体版本及传递依赖以 Cargo.lock 为准：

- [Axum](https://github.com/tokio-rs/axum)、[Tokio](https://github.com/tokio-rs/tokio)、[tokio-util](https://github.com/tokio-rs/tokio)：HTTP 服务、异步 IO、任务取消。
- [reqwest](https://github.com/seanmonstar/reqwest)、[rustls](https://github.com/rustls/rustls)：连接池、HTTP 流式读取及 TLS。
- [futures-rs](https://github.com/rust-lang/futures-rs)：有界并发调度。
- [Serde](https://github.com/serde-rs/serde)、[serde_json](https://github.com/serde-rs/json)：JSON。
- [anyhow](https://github.com/dtolnay/anyhow)：错误上下文。
- [uuid](https://github.com/uuid-rs/uuid)：任务 ID；固定 1.18.1 以兼容 Rust 1.88 构建镜像。
- [RustCrypto SHA-2](https://github.com/RustCrypto/hashes)：流式 SHA-256。

没有内嵌 rclone、付费解析服务或第三方账号凭据。v0.6 起，Docker 通过 Debian 软件包安装独立 aria2 进程处理普通链接与 BT，不链接到 Rust 二进制。发布二进制或镜像时，应随其附上适用的依赖许可证；通过网络提供被修改的 OpenList 时，还须遵守该项目 AGPL 的源代码提供义务。此处仅记录组件边界，不改变各项目许可证。

## v0.2 认证接口参考

参考的是上游 HTTP 协议和官方登录页的消息格式，没有复制第三方浏览器自动化实现：

- [夸克官方短信表单](https://uop.quark.cn/cas/custom/login?custom_login_type=mobile&client_id=532&display=pc)：用户浏览器加载，成功后发送 service ticket；服务端交换 Cookie。
- [QuarkPan 二维码认证实现](https://github.com/lich0821/QuarkPan/blob/main/quark_client/auth/api_login.py)：核对官方 Token、轮询、ticket、account/info 流程。
- [OpenList 认证助手 115 二维码接口](https://github.com/OpenListTeam/OpenList-APIPages/blob/main/src/driver/115cloud_qr.ts) 与 [115driver 二维码交换协议](https://github.com/SheltonZhu/115driver/blob/master/pkg/driver/qrcode.go)。
- [OpenList 百度 OAuth 实现](https://github.com/OpenListTeam/OpenList-APIPages/blob/main/src/driver/baiduyun_oa.ts)：核对百度官方授权、code 交换及刷新令牌字段。
- [qrcode-rust](https://github.com/kennytm/qrcode-rust)，qrcode 0.14.1，MIT/Apache-2.0：在 Rust 内生成 SVG，不向外部二维码图片服务泄露登录票据。

二维码和票据不会写入源码或测试报告。Playwright 仅用于开发时验证 UI，不属于部署依赖。

## v0.3 百度 Cookie 接入参考

新增 Rust 原生百度适配器，参考接口协议，不编译、链接或运行参考项目：

- [tvbox-silicon 的 CloudDriveQRLogin](https://github.com/jo32/tvbox-silicon/blob/main/Sources/TVCore/CloudDriveQRLogin.swift)：官方 `getqrcode`、`channel/unicast` 和 `qrbdusslogin` 的请求字段、JSONP 与 Cookie 交换顺序。
- [BaiduPCS-Go](https://github.com/qjfoidnh/BaiduPCS-Go)：本人 Cookie 下的目录、文件操作和原文件下载协议；[分享转存](https://github.com/qjfoidnh/BaiduPCS-Go/blob/main/baidupcs/transfer.go) 的分享页面字段、提取码、`BDCLND`、`share/list`、`share/transfer`。
- [base64](https://github.com/marshallpierce/rust-base64)，MIT/Apache-2.0：把官方返回的二维码 PNG 放进已鉴权的登录会话；不接第三方图片服务。

仅用于本人账号授权访问。没有使用第三方解析服务、共享会员、验证码求解器或远程浏览器。


## v0.3 补充协议研究

- [komorebiCarry/BaiduPCS-Rust](https://github.com/komorebiCarry/BaiduPCS-Rust)，Apache-2.0：核对 `netdisk/client.rs` 中 PCS Locate 取链协议，以及 README 的分段上限、普通账号并发建议。本项目独立实现对应协议及有界恢复，不链接或编译该项目。
- [迅雷官方个人账号页面](https://i.xunlei.com/center/account/personal/) 及 [迅雷云盘](https://pan.xunlei.com)：核对公开客户端的设备码授权、二维码跳转、token 轮询和错误状态。仅调用官方 HTTPS 接口，不加载或打包其脚本；不把用户授权交给第三方。
- [OpenList 上传接口](https://github.com/OpenListTeam/OpenList/blob/v4.2.6/server/handles/fsup.go)：核对 File-Path、Overwrite=false 和同步保存参数。


## v0.3.1 迅雷登录修正

继续核对迅雷官方个人账号页面的设备码确认页，以及官方 `shield/captcha/init`、`auth/verification`、`auth/verification/verify`、`auth/signin` 协议。独立实现手机号表单、验证码动作及服务端状态，不运行或打包官方前端 SDK。短信不会因打开弹窗自动发送；验证组件由用户直接操作官方页面，无远程桌面或验证码自动求解。

## v0.3.2 迅雷分享接入

核对[迅雷官方云盘](https://pan.xunlei.com)前端对 `share`、`share/detail`、`share/restore`、`tasks/{id}` 和 `files` 的实际调用及响应；同时以 [ctwj/urldb 的协议实现](https://github.com/ctwj/urldb/blob/main/common/xunlei_pan.go)交叉检查转存请求字段。仅独立实现官方 HTTP 协议，不运行、复制或打包这些项目的脚本或 SDK。

新增 [md5](https://github.com/stainless-steel/md5)（MIT/Apache-2.0），仅用于上游要求的接口签名；下载文件完整性继续使用 SHA-256。账号刷新令牌变化会先同步回 OpenList，再继续转存；转存提交前记录意图，响应丢失时停止自动重复提交。

## v0.4 分享浏览与预览

复用已有网盘协议和 OpenList 的元数据、链接及请求头，不引入前端组件或媒体处理依赖。媒体由浏览器原生图片、音频和视频控件显示；文本以纯文本渲染。直接读取百度 `api/quota`、夸克 `member`、迅雷 `drive/v1/about` 的容量字段，不依赖旧版网关一定返回容量详情。广告与密码备注采用本项目本地名称规则，不请求外部 AI 或解析服务。

## v0.5 OneDrive / Google Drive（2026-10-21）

账号文件管理复用已部署 OpenList v4.2.6 的 [OneDrive 驱动](https://github.com/OpenListTeam/OpenList/tree/v4.2.6/drivers/onedrive) 与 [Google Drive 驱动](https://github.com/OpenListTeam/OpenList/tree/v4.2.6/drivers/google_drive)，通过 HTTP API 操作，未复制驱动代码或增加 Rust 依赖。默认应用令牌通过 [OpenList 授权助手](https://api.oplist.org/) 取得及刷新；也可使用自建 OAuth 应用直接向官方交换及刷新令牌。

Rust 的分享只读视图与授权回调按官方协议独立实现：

- [Microsoft Graph 委托授权](https://learn.microsoft.com/en-us/graph/auth-v2-user)；请求离线访问与文件管理权限。
- [读取共享 DriveItem](https://learn.microsoft.com/en-us/graph/api/shares-get?view=graph-rest-1.0)：URL-safe Base64 分享标识，按需授权读取分享，不复制文件。
- [Google Web Server OAuth](https://developers.google.com/identity/protocols/oauth2/web-server)：offline access、同意授权、后端交换刷新令牌。
- [Google Drive resource keys](https://developers.google.com/workspace/drive/api/guides/resource-keys)：资源访问键仅通过后端请求传递。

微软和 Google 的真实账号、管理员授权和实际限流行为没有通过模拟接口验证，不将协议模拟结果表述为真实账号登录完成。

## 聚合下载引擎

[aria2](https://github.com/aria2/aria2)，Docker 使用 Debian bookworm 的 `aria2=1.36.0-1` 软件包。aria2 按 GPL-2.0-or-later 独立分发，许可证与源码包见 [Debian 包页面](https://packages.debian.org/bookworm/aria2) 和镜像中的 `/usr/share/doc/aria2/copyright`。Rust 程序经本机 JSON-RPC 控制独立进程，不复制或链接其代码。公开源码包仅包含 FlowDisk 原创代码、构建文件与说明；重新分发镜像及 aria2 软件包时仍须遵守上游许可。

[scraper](https://github.com/rust-scraper/scraper) 0.24.0（ISC）用于静态 HTML 解析、CSS 选择器及链接实体解码；不运行远程网页脚本。
