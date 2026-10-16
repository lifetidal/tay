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

## 已知问题与遗留

- **锁屏时渲染阻塞**:KWin 锁屏后不再发帧回调,Mesa WSI 的
  acquire/present 会永久阻塞(主线程卡在渲染)。解锁后自愈。
  layer-shell 无遮挡信号,根治需 M4+ 的 occlusion 探测或超时
  watchdog。验证视觉输出时请保持会话未锁定。
- 图像放置的 c/r 小数偏移、x/y 像素偏移、z 更精细分层
- 打包(Flathub/AUR/nixpkgs/COPR)与配置向导
