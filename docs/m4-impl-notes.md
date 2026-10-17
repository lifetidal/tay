# M4 实施笔记(进行中,2026-10-16)

M4 = 生态(§9):图像协议、打包、配置向导。

## 已完成:kitty graphics 图像显示

- **流拦截**:ptyloop 扫描器识别 APC(ESC _ … ST),kitty 序列
  (`G<控制键>;<载荷>`)解析后从流中**剥离**,终端收到的是干净流;
  C=1(默认)语义由前端合成 `\r\x1b[NB` 注入回解析器(光标移到图像下方)。
- 协议支持:t/T/p/P/d 动作(大小写语义)、f=100 PNG(base64)/f=32 RGBA/
  f=24 RGB、s/v 像素尺寸、c/r 显式单元格、i/I id、z 层级、p=1 先删后放。
  非 kitty 的 APC 原样转发;畸形序列按协议丢弃。单测 5 个。
- 渲染:tay-render 新增纹理管线(image.wgsl)+ 内容 hash 纹理缓存;
  z<=0 画在文本下、z>0 画在文本上;折叠感知(跨折叠区间隐藏)。
- 图像锚定网格行,随滚动/显示窗口移动;同 id 重传 = 替换;删除 a=d 支持。
- **两个深坑**:
  1. 两个管线都曾是 4 顶点 + TriangleList(默认拓扑)= 只画半个 quad ——
     背景色块一直靠「clear 色 == 背景色」掩盖,图像暴露了它。修:6 顶点。
  2. 多图共用一个实例缓冲:同帧多次 write_buffer 都在 pass 前排队,
     GPU 执行时所有 draw 读到最后一个 rect。修:每图独立实例缓冲。
- 实测:48×48 PNG 与 96×48 渐变双图并存渲染(4640px + 10189px);
  性能无退化(1143fps)。

## 已完成:sixel + iTerm2 inline(§7 P1 三件套齐了)

- **sixel**:扫描器新增 DCS 状态;判定靠 icy_sixel 解码成败
  (参数可出现在 q 之前,如 `P1;P2;P3 q`),非 sixel 的 DCS 原样转发;
  单测覆盖(编码器输出 → 扫描器 → 图像事件,与 icy 解码结果一致)
- **iTerm2 inline**:OSC 1337 File=inline=1,base64 PNG 解码,
  宽高取显式 px 否则取内禀尺寸;非 inline 的 File 转发
- make_sixel 示例:PNG → sixel 序列(测试素材生成)

## 修正:sixel 光标语义 + make_sixel 素材生成

- sixel 显示后光标下移到图像下方(VT340 惯例),iTerm2 inline 右移
  N 列 —— 引入 CursorAfter{None,Down,Right} 枚举统一三种协议;
  此前两个 sixel 叠在同一位置互相覆盖
- make_sixel 示例修正:icy_sixel 的 encode() 自带完整 DCS 包装,
  此前重复包装产生嵌套畸形序列(解码成黑色)
- 实测:sixel 红方块 4640px + 渐变 8603px 并存,位置正确错开

## 已完成:occlusion watchdog + 多显示器 output 配置

- **occlusion watchdog**:帧回调 2s 中断判定为合成器遮挡/锁屏,
  暂停渲染请求(丢弃滞留请求防帧风暴);事件循环继续运转
  (IPC/热键/动画不受影响);回调恢复后自愈。根治了此前
  「锁屏 → Mesa WSI acquire 永久阻塞主线程」的问题
- **多显示器 output 配置**:config.output(显示器名),layer surface
  绑定指定输出;名字无效时告警并退回合成器默认。实测通过。
  (多屏实机验证待有第二显示器时进行)
## 已完成:打包初版

- packaging/:AUR PKGBUILD(tay-git,git source,cargo release)+
  systemd user 单元(graphical-session.target,Restart=on-failure,
  DESIGN §4.3 的登录自启)+ tay.desktop + 安装说明
- LICENSE(GPL-3.0,SPDX 全文)—— 补齐仓库缺失
- 验证:PKGBUILD bash -n ✓、desktop-file-validate ✓、
  systemd-analyze verify ✓(仅缺已安装的 tayd 二进制)

## 遗留

- 图像放置的 c/r 小数偏移、x/y 像素偏移、z 更精细分层
- Flatpak 沙箱全链路实测(M4 验收项)、nixpkgs/COPR
- 配置向导、GNOME 扩展伴侣(M2 遗留)
