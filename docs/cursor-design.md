# 鼠标指针合成设计

> 状态：**已实现**。对应 `capture/cursor.rs`（形状解析与 CPU 合成）、`capture/dxgi.rs`
> （指针状态机）、`encode/mf_sink.rs`（显存内合成）。
> ABI 无需变更：`RecorderConfig.capture_cursor` 从 v1 起就在，此前一直被忽略。

## 1. 问题

`IDXGIOutputDuplication` 交付的桌面图像**不含鼠标指针**。指针走的是完全独立的旁路：

- `DXGI_OUTDUPL_FRAME_INFO.PointerPosition` —— 位置 + 可见性；
- `DXGI_OUTDUPL_FRAME_INFO.PointerShapeBufferSize` —— 形状是否变化；
- `IDXGIOutputDuplication::GetFramePointerShape` —— 现取形状的像素数据。

也就是说，想要「录屏里有鼠标」，必须自己把形状画到画面上。这不是可选优化，是唯一路径。

## 2. 调用时序（错一步就废）

```text
AcquireNextFrame(…, &frame_info, &resource)
    ├─ ① update_pointer(&frame_info)        ← 必须在 ReleaseFrame 之前
    │      ├─ LastMouseUpdateTime != 0 → 更新位置/可见性
    │      └─ 需要形状 → GetFramePointerShape(…)
    ├─ ② 用 frame_info.AccumulatedFrames 判断桌面是否真的变了
    └─ ③ 裁剪 / 合成 / 送编码器
ReleaseFrame()
```

`GetFramePointerShape` 要求**调用方仍持有帧**，否则返回 `DXGI_ERROR_INVALID_CALL`。
所以指针刷新只能在拿到帧之后、`ReleaseFrame()` 之前那一段窗口里做。

## 3. 五个契约（每条都踩过或查证过）

### 3.1 `Position` 是光标图像的左上角，不是热点

官方文档对 `DXGI_OUTDUPL_POINTER_SHAPE_INFO` 的备注写得很直白：

> 应用程序在 `Position` 指定的位置绘制光标形状的**左上角**；应用程序**不使用**热点。

按常识减掉 `HotSpot` 会让指针整体偏移，而且偏移量随光标样式变化（箭头是 `(0,0)`，
文本 I 型常是 `(4,8)` 之类）——表现出来就是「某些指针位置对、某些错位」，非常难查。
`HotSpot` 我们只原样透出（`CursorShape::hotspot()`），合成时一律不用。

### 3.2 不是每帧都带指针更新

`PointerPosition` 只在 `LastMouseUpdateTime != 0` 的帧里有效，其余帧该字段全 0。
若无脑采信，鼠标一停下指针就会瞬移到区域左上角然后消失。

因此位置与形状都**持久化在 `DxgiCapture` 上**，只在有更新时覆盖。

### 3.3 `PointerShapeBufferSize > 0` 才代表形状「变了」——但形状为空时必须主动探

前半句是文档语义：形状没变时为 0，此时重复调 `GetFramePointerShape` 是浪费。

后半句是实测补出来的。**只在 `PointerShapeBufferSize > 0` 时才取形状，会漏掉开头几十帧**：

```text
帧 #1..#20  state=visible  shape=None      ← 位置一直在更新，形状始终拿不到
帧 #21..#39 state=visible  shape=None
```

根因是「刚建立复制会话时，形状不一定随着第一个鼠标更新一起给出」。改成
`shape.is_none()` 时无条件主动探一次之后，同一场景第 3 帧就拿到了 `32×32`。

主动探取用一个 64 KB 的起步缓冲；真装不下时 API 返回 `DXGI_ERROR_MORE_DATA` 并回填
真实所需大小，再按它重试一次（**不要猜大小**）。

### 3.4 单色光标是 1bpp 双掩码，不是 BGRA

缓冲区布局是「AND 掩码 `Height` 行，紧接 XOR 掩码 `Height` 行」，行距都是 `Pitch`。
把它当 32bpp 解读会得到一片花屏。

解析时以「缓冲区实际够不够高」为准：

```rust
let rows = data.len() / pitch;
let height = if declared_h * 2 <= rows { declared_h } else { rows / 2 };
```

这样 `Height` 给的是单掩码高度（正常驱动）还是两倍（个别驱动）都能吃下。

### 3.5 `MASKED_COLOR` 的枚举值是 **4**，不是 3

DXGI 在这个枚举里跳过了 3。凭记忆写 `3` 会静默落到「未知类型」分支，表现为
「某些光标永远画不出来」。`windows` crate 里的常量是
`DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 == 4`，直接用常量、别写字面量。

## 4. 三种形状的合成公式

| 类型 | 数据 | 公式 |
|---|---|---|
| `MONOCHROME` | 1bpp AND + 1bpp XOR | `out = (screen & AND) ^ XOR`，逐通道按 `0x00`/`0xFF` 展开 |
| `COLOR` | 32bpp BGRA | 直通 alpha 混合 `out = src*a + screen*(1-a)` |
| `MASKED_COLOR` | 32bpp BGRA，alpha 当掩码 | `a < 0x80` → 光标色不透明；`a >= 0x80` → `screen ^ 光标色` |

单色掩码的四种组合正好对应经典语义：

| AND | XOR | 结果 |
|---|---|---|
| 1 | 0 | 透出桌面 |
| 1 | 1 | 桌面反色 |
| 0 | 0 | 纯黑 |
| 0 | 1 | 纯白 |

位序是 **MSB 优先**（DDB 位图惯例）：`bit = (byte >> (7 - (x & 7))) & 1`。

`COLOR` 用真正的 alpha 混合而不是「过阈值就当不透明」：现代抗锯齿光标的边缘有中间
alpha 值，阈值法会让边缘出现硬锯齿。

## 5. 架构：一份像素逻辑，两条搬运路径

```text
                    ┌─ CPU 路径：acquire_frame()
DxgiCapture ────────┤     桌面缓存 → to_vec() → blend_into() → CapturedFrame
（指针状态机）       │
                    └─ GPU 路径：acquire_texture()
                    FrameMeta{ desktop_updated, OverlayPlan }
                                    ↓
                          GpuSinkWriter::push_frame()
                             desktop_texture ─┐
                                              ├─ CopyResource → cropped_texture
                             bounds 回读 ← ────┘
                             draw_into() → 写回 cropped_texture
                                    ↓
                             BGRA→NV12 → 编码器
```

关键决定：**像素逻辑只有一份**（`CursorShape::blend_into_alpha` 与
`OverlayPlan::draw_into`），两条路共用。GPU 侧不另写着色器。

## 6. 为什么 GPU 路径不用着色器

三种方案对比：

| 方案 | 代价 | 问题 |
|---|---|---|
| ① 整帧回读到 CPU 合成 | 4K 每帧 33 MB PCIe | 等于把零拷贝的收益全吐回去 |
| ② 着色器绘制光标四边形 | 需要 `D3DCompile`（d3dcompiler_47.dll）+ 顶点/像素着色器 + 混合状态 | 反色分支必须读目标像素，而「读 A 写 A」不允许；要么再复制一份，要么把桌面当 SRV 再采一次。像素逻辑变成第二份实现，必然与 CPU 路径分歧 |
| ③ **只回读光标包围盒** | 每帧一次小 Map（约 0.1–0.3 ms） | 无 |

选 ③。包围盒通常只有 32×32～64×64，回读量是整帧的 **1/3600**（1080p）。
顺带解决反色分支：桌面像素本来就在手边。

代价是一次 `Map` 会同步 GPU 队列。实测 320×240 场景合起来仍在 0.5 ms 量级，
相对 30 fps 的 33 ms 预算是 1.5%。

## 7. 两份纹理：残影是从哪来的

`cropped_texture` 每帧都会被编码。如果直接在上面叠加光标，**上一帧的光标不会被擦掉**——
鼠标快速划过会拖出一串箭头残影。而单帧截图完全看不出来，只有播放时才现形。

所以需要：

- `desktop_texture` —— 只含桌面，仅当 `desktop_updated` 时更新；
- `cropped_texture` —— 每帧从 `desktop_texture` **重新派生**一次（`CopyResource`），再画光标。

`CopyResource` 是显存内 DMA，1080p 约 0.1 ms。

只在开启指针合成时才创建 `desktop_texture`；关闭时走的还是原来的「直接裁剪」一条路，
不引入任何额外拷贝。

## 8. 边界条件

| 情形 | 处理 |
|---|---|
| 光标部分在区域外 | 包围盒与画面求交，`blend_into` 自己裁列/行 |
| 光标完全在区域外（鼠标在另一块屏） | `OverlayPlan::bounds` 返回 `None`，一个像素都不写，不触发重编码 |
| 指针不可见（`Visible == FALSE`） | 不画。注意不能「保留上一次的指针」 |
| 区域跨屏 / 坐标为负 | 与既有 `find_target` 的裁剪逻辑一致，指针坐标同步减去 `offset_x/offset_y` |
| 设备丢失后重建 | `rebuild()` 清空 `desktop_bgra` / `shape`，并置 `force_desktop_copy` |
| 形状尺寸变化 | GPU 侧按新尺寸重建暂存纹理；`changed = true` 保证当前帧重画 |

### 8.1 「只有指针动」的帧

`AccumulatedFrames == 0` 表示桌面图像没变，但 `AcquireNextFrame` 仍会为指针更新返回成功。
这正是鼠标在静止画面上移动的情形，必须产出新帧，否则指针会冻住。

- CPU 路径：用缓存的 `desktop_bgra` 复制一份再合成，不重新读显存。
- GPU 路径：跳过源纹理的拷贝（`desktop_updated == false`），只做
  `desktop_texture → cropped_texture` + 画光标。

反之，若桌面没变、指针也没动，`acquire_frame` 直接返回 `Ok(None)`（等价
`WAIT_TIMEOUT`），由上层重发上一帧副本——顺带省掉一次整帧 memcpy。

### 8.2 `force_desktop_copy`

判据不能只看 `AccumulatedFrames > 0`：`DuplicateOutput` / 重建后的第一帧被我们丢弃
（那是未初始化的桌面图像），若紧接着来一帧「只有指针动」，`AccumulatedFrames` 会是 0，
而缓存还是空的。所以首帧与重建后强制置位，直到成功交付一帧才清。

## 9. 与其它子系统的交互

- **切片**：指针烧进画面像素，所以切片天然带着指针，不需要额外处理。
- **音频**：无交互。指针帧和普通帧走同一条 PTS 规则。
- **静态画面 CFR**：指针一帧一帧地动，帧就被一帧一帧地编码；画面和指针都静止时
  走重复帧路径，码率不会因为开了指针而暴涨。
- **ABI**：`capture_cursor` 与 v3 的 `cursor_click_effect` / `cursor_trail_ms`（见
  [`cursor-effects.md`](cursor-effects.md)）。

## 10. 性能

| 项 | 相对开销 |
|---|---|
| CPU 路径：整帧 `to_vec()` | 1080p 约 1 ms（原本就要整帧读回，只是多一次拷贝） |
| GPU 路径：`desktop→cropped` CopyResource | 1080p 约 0.1 ms |
| GPU 路径：bbox 回读 + Map | 约 0.1–0.3 ms（与光标尺寸相关，与画面尺寸无关） |
| 指针静止且画面静止 | 0（走重复帧） |

## 11. 测试矩阵

| 用例 | 覆盖 |
|---|---|
| `mono_mask_implements_all_four_and_xor_cases` | 单色四种组合（含反色） |
| `mono_masks_are_two_stacked_blocks_not_interleaved` | 双掩码是两段而不是行交错 |
| `mono_rejects_truncated_buffer` / `mono_rejects_pitch_too_small_for_width` | 坏缓冲区不越界读 |
| `color_blends_half_transparent_alpha` / `color_handles_fully_transparent_and_opaque` | alpha 混合精度 |
| `masked_color_uses_alpha_as_mask` | masked-color 的两个分支 |
| `clipping_keeps_in_frame_part_and_skips_the_rest` / `fully_outside_returns_zero_and_writes_nothing` | 越界裁剪 |
| `hotspot_does_not_shift_placement` | **位置是左上角**，热点不参与 |
| `bounds_is_union_and_clipped`（`overlay`） | 包围盒是并集、且裁到画面内 |
| `unknown_shape_type_is_ignored` / `zero_sized_shape_rejected` | 退化输入不 panic |
| `gpu_cursor_is_composited_and_leaves_no_trail`（`#[ignore]`） | 显存内合成：位置对、无残影、不可见不画、动画帧可重绘并抹净 |
| `e2e_cursor.rs` 两个用例（`#[ignore]`） | 真实桌面：真取到形状、真移动到编码像素上；关闭开关时完全不采 |

## 12. 实现复盘

### 12.1 主动探形状是被实测逼出来的

最初严格按文档只在 `PointerShapeBufferSize > 0` 时取。真实桌面跑出来的结果是连续
四十多帧「位置在更新、形状为空」——`capture_cursor = true` 但画面里没有指针。
改成 `shape.is_none()` 时无条件探一次，立刻在第 3 帧拿到形状。

**教训**：`PointerShapeBufferSize` 的语义是「形状**变了**」，不是「形状**可用**」。
把「变化通知」当成「可用性判据」是很自然的误读，而它的症状（开头几秒没指针）在
单元测试里根本不可见。

### 12.2 测试断言不能写死坐标

`e2e_cursor.rs` 一开始断言「采集到的指针位置 == 我们 `SetCursorPos` 的位置」，
在开发机上稳定失败——因为**用户正在用鼠标**，位置一直在漂。实测 `SetCursorPos(60,60)`
之后读回 61、59、54 都出现过，甚至整个被挪到 `(1257,36)`。

改法：断言一律以「采集到的实际位置」为准，只要求两次采样相距 ≥ 64 px
（大于比对块 48 px），并检查两处像素块都发生变化。

**教训**：跑在用户正在使用的机器上的测试，任何「输入 → 状态」的精确对应都可能被外力
打断。要断言的是因果关系（指针动了 → 那两块像素就该变），不是数值相等。

### 12.3 视觉验证省了一轮猜

单测能证明公式对，但证明不了「屏幕上看起来对」。把光标附近 96×96 存成 BMP 看了一眼，
确认箭头尖端正好落在 32×32 框的左上角、没有因热点偏移——这类问题只有眼睛能发现。
（诊断代码用完即删，不留进仓库。）

## 13. 已知边界

- **GDI 首帧兜底没有指针。** `BitBlt` 从不把光标画进结果，而形状只能在持有 DXGI 帧时
  取到。这条兜底最多用一次，紧随其后的 DXGI 帧就有指针，不值得为它再开一次桌面复制。
- **纯硬件覆盖层内容看不到。** 部分全屏独占游戏把画面和光标都放在覆盖层，桌面复制
  本身就看不到，与 `capture_cursor` 无关。
- **旋转显示未特殊处理。** 与既有的区域映射一致（沿用 `find_target` 的坐标假设）。
- 指针是**烧进像素**的，录完无法关闭。
