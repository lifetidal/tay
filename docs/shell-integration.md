# Shell 集成(OSC 133 命令块)

Tay 的命令块(DESIGN §8.4)依赖 OSC 133 标记:

| 序列 | 含义 | Tay 用途 |
|---|---|---|
| `ESC ] 133 ; A ST` | 提示符开始 | 块边界、折叠锚点、提示符跳转 |
| `ESC ] 133 ; C ST` | 命令输出开始 | 「按块复制」只复制纯输出(跳过回显的命令行) |
| `ESC ] 133 ; D ; <code> ST` | 上一条命令退出码 | 行尾徽章(非零显示红色代码) |

- **fish 4.9.3(2026-10-19 实测,逐字节抓包)不再发送 OSC 133**:
  「fish 4 零配置」不成立,需用下方钩子;更早/更新版本以实测为准。
- **bash / zsh** 需要自行挂钩,片段如下。
- **alt-screen 守卫**:vim / less / tmux 等全屏应用内部的 133 标记
  不产生命令块(P3b)。
- **锚点失效与恢复**(2026-10-17 加固):块锚点是**网格绝对行号**,
  滚动回看不失效;以下事件使锚点失效并自动清空(下一条 prompt 重建):
  - 清屏/重置(ED2/ED3/RIS 序列);
  - resize/reflow(调整列宽/行数);
  - 历史淘汰(行距超出滚动历史上限,最老锚点被裁剪)。
- **复制语义**(`last_output_range`,`Ctrl+Shift+O` 按块复制):
  - 复制范围 = [C 行, 下一提示符前一行],含两端;**单行输出可复制**;
  - 空输出(无任何输出行)返回空;
  - **无末尾换行**:提示符与输出同行 —— 复制到提示符前一列,
    输出完整且不含提示符(2026-10-17 第三轮修复);
  - 多行提示符不影响范围(C 在命令回显之后发出);
  - 复制文本不含末行行终止符(bounds_to_string 语义)。

## bash

`~/.bashrc`:

```bash
__tay_precmd() {
    # 提示符开始(A 在 PS1 渲染前发出)
    printf '\033]133;A\007'
}
__tay_preexec() {
    # 命令开始(可选:标记回显段开始)
    printf '\033]133;B\007'
    # 输出开始(C 紧跟命令回显之后)
    printf '\033]133;C\007'
}
__tay_exit() {
    printf '\033]133;D;%s\007' "$?"
}

# bash ≥ 4.4 用 PS0(preexec),否则用 DEBUG trap
if [ "${BASH_VERSINFO[0]}" -ge 5 ] || { [ "${BASH_VERSINFO[0]}" -eq 4 ] && [ "${BASH_VERSINFO[1]}" -ge 4 ]; }; then
    PS0='$(__tay_preexec)'
else
    trap '__tay_preexec' DEBUG
fi
PROMPT_COMMAND='__tay_exit; __tay_precmd'
```

> 注意:`C` 应恰好在命令回显**之后**发出(上述 PS0/DEBUG trap
> 在命令执行前、回显已发生时运行,正合要求);若 C 早于回显,
> 「按块复制」会包含命令行本身。

## fish

fish 4.9.3 起不再内置发送 OSC 133(实测),`~/.config/fish/config.fish`
挂三个事件(fish ≥ 3.5;`$pipestatus` 取管道最后一个退出码):

```fish
function __tay_precmd --on-event fish_prompt
    printf '\e]133;A\a'
end
function __tay_preexec --on-event fish_preexec
    printf '\e]133;C\a'
end
function __tay_postexec --on-event fish_postexec
    printf '\e]133;D;%s\a' $pipestatus[-1]
end
```

## zsh

`~/.zshrc`:

```zsh
autoload -Uz add-zsh-hook

tay_preexec() {
    print -n '\033]133;C\007'
}
tay_precmd() {
    print -n '\033]133;A\007'
}
tay_exit() {
    print -n "\033]133;D;$?\007"
}

add-zsh-hook preexec tay_preexec
add-zsh-hook precmd tay_exit
add-zsh-hook precmd tay_precmd
```

## 验证

```bash
tayd &            # 启动 daemon
tay show
false             # 应看到行尾红色徽章 "1"
# Ctrl+Shift+O   # 复制上一条命令的纯输出(不含命令行)
```

也可直接注入序列验证解析:

```bash
printf '\033]133;A\007'; printf '\033]133;C\007'; echo out; printf '\033]133;D;7\007'
```
