# 在 Mac 上继续开发（Duo 回国方案）

本仓库基于 [jesther-ai/open-u60-pro](https://github.com/jesther-ai/open-u60-pro)（MIT）。第一个提交是上游 `5596b8d` 的原样快照，之后的提交是 Duo 相关改动，可用 `git diff 33a653a..` 查看全部差异。

## 一键准备环境

```bash
git clone https://github.com/cuteSakuraHime/u60pro-duo.git
cd u60pro-duo
./scripts/bootstrap-mac.sh
```

脚本可重复运行，会依次：

1. 检查 Xcode 版本（固定为 `.xcode-version` = 26.6，需先从 App Store 安装并打开一次）
2. `brew bundle` 安装 `Brewfile` 中的工具（XcodeGen、rustup、musl 交叉编译器、adb、Python、Node）
3. 按 `rust-toolchain.toml` 安装 Rust 1.91.1 和 `aarch64-unknown-linux-musl` 目标
4. 生成本地配置 `mobile/ios/OpenU60/.env`、`tools/u60.env`（都不进 git）
5. 用 XcodeGen 生成 `OpenU60.xcodeproj`（不进 git，`project.yml` 是唯一来源）
6. 运行离线测试 + 模拟器编译 + agent 单元测试，全部通过即环境复刻成功

## 必须手动填写的本地配置

`mobile/ios/OpenU60/.env`：

| 变量 | 说明 |
|---|---|
| `DEVELOPMENT_TEAM` / `TEAM_ID` | Apple 开发者 Team ID，真机必填，模拟器可空 |
| `BUNDLE_ID_PREFIX` | 你账号下唯一的 Bundle ID，例如 `com.yourname.openu60` |

改完后重新生成工程：`cd mobile/ios/OpenU60 && set -a && . ./.env && set +a && xcodegen generate`

`tools/u60.env`：路由器 IP、adb 路径、唯一允许拨打的测试号码。

## 常用命令

```bash
# iOS
mobile/ios/scripts/build_and_run.sh --simulator      # 模拟器运行
mobile/ios/scripts/build_and_run.sh                  # 真机（需 TEAM_ID）
sh scripts/test-ios-duo.sh                           # 短信验证码、通话记录、RTP/录音测试

# 路由器 agent（Rust）
cargo test -p zte-agent telephony
cargo build --release --target aarch64-unknown-linux-musl -p zte-agent
./deploy.sh                                          # 部署到已连接的 U60 Pro

# 路由器调试工具（需 USB adb 或同一局域网）
python3 -m unittest discover -s tools -p 'test_*.py' # 离线测试，不连设备
python3 tools/qmi_dial.py                            # 拨打 U60_TEST_NUMBER，确认状态后挂断
```

## 当前进度（2026-11-10）

| 功能 | 状态 |
|---|---|
| U60 Pro 真实拨出 | 已验证：解锁 NV 5280 后电话可振铃，挂断已确认。解锁工具见 [u60pro-voice-unlock](https://github.com/cuteSakuraHime/u60pro-voice-unlock) |
| 短信推送 + 验证码识别 | 代码完成，有离线测试；通知不含短信正文与验证码 |
| CallKit 接听 / App 内拨出 | 代码完成，需真机验证 |
| 通话记录 | 代码完成，有离线测试；仅保存在本机、不进备份 |
| RTP / G.711 解码与录音文件 | 解析与 WAV 写入完成并有测试；尚未接入实际通话音频 |
| 双向通话音频 | 进行中：路由器 `libztertp.so` 在 UDP 48894 收发 PCMU，需在通话中实测 |
| App 被挂起时接来电 | 未定：iOS 只能用 PushKit（经 Apple 推送服务器）唤醒，与"不经第三方服务器"冲突 |

关键文件：

- `mobile/ios/OpenU60/Features/Call/`：`CallKitManager`、`CallViewModel`、`CallHistory`、`CallAudio`
- `mobile/ios/OpenU60/Features/SMS/SMSModels.swift`：验证码识别 `SMSParser.verificationCode`
- `zte-agent/src/telephony.rs`：`/api/call/status` 严格解析 `AT+CLCC`
- `tools/`：QMI 拨号、RTP 探测、音频日志脚本

## 不在仓库里的东西

固件镜像、基带/NV 备份、设备日志、厂商二进制、密码与 Token 都没有上传。需要时从设备重新读取，不要提交。
