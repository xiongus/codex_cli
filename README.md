# codex_cli

一个极简的 Codex 账号池工具。

它只做两件事：

- `add`：把当前 `~/.codex/auth.json` 加入账号池
- `auto`：实时探测账号池中每个账号的官方额度信息，并自动切换到当前更合适的账号

账号池数据统一存放在：

```text
~/.codex/account-pool/accounts/
```

本仓库不会存储任何账号凭据。账号文件只保存在你的本机 `~/.codex` 目录中。

## 要求

- 已安装 Codex Desktop 或 Codex CLI，并且命令 `codex` 可用
- Python 3.10+

## 安装

在仓库目录执行：

```bash
./install.sh
```

安装后会得到全局命令：

```bash
codex-pool
```

如果 `~/.local/bin` 还没加入 `PATH`，请按安装脚本提示补上。

## 使用

先把当前登录账号加入账号池：

```bash
codex-pool add
```

自动探测并切换到当前最优账号：

```bash
codex-pool auto
```

`auto` 会输出每个账号的实时检测结果，内容来自 Codex 官方 app-server 能力，包括：

- 邮箱
- 账号 plan
- 5 小时额度剩余
- 周额度剩余
- 5 小时窗口重置时间
- credits 状态

示例输出：

```text
检测账号池，共 3 个账号
[1/3] user1@example.com  plan=business  5h=94% left  weekly=64% left  reset_5h=04-08 16:17  credits=no  status=usable
[2/3] user2@example.com  plan=team      5h=20% left  weekly=12% left  reset_5h=04-08 13:05  credits=no  status=usable
[3/3] user3@example.com  plan=business  5h=0% left   weekly=8% left   reset_5h=04-08 12:42  credits=no  status=full

selected: user1@example.com
switched auth -> /Users/yourname/.codex/auth.json
```

## 卸载

```bash
./uninstall.sh
```

这只会移除全局命令，不会删除你的账号池数据。

## 安全说明

- 仓库内不会提交任何 `auth.json`、token 或账号缓存
- 历史 `tokens/` 目录应仅做本地迁移用途，不应进入 git
- 账号池中的凭据文件默认只保存在本机 `~/.codex/account-pool/accounts/`
