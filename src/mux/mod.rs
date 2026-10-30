//! 封装模块。
//!
//! 录制中的形态有三类，差别只在「盒是谁写的」——三者产物同构，都是抗崩溃的分片
//! MP4，停止时做同一套 16 字节 soft remux 收敛成普通 MP4：
//!
//! - 软编：[`mp4::Mp4Muxer`] 自己写盒，停止时直接补 moov + 覆写 mdat 头。
//! - 硬编 H.264：盒由 Media Foundation 分片 sink 写，停止后由
//!   [`remux::finalize_fragmented`] 扫一遍分片、重建 moov、做同一套 soft remux。
//! - 硬编 HEVC：盒由自研 `IMFMediaSink`（`crate::encode::sink_raw`）交给
//!   [`mp4::Mp4Muxer`] 写。MF 的分片 sink 只接受 H.264，所以把「写盒」这一层
//!   拿回来自己做——编码器仍然由 `IMFSinkWriter` 挑选和驱动。
//!   代价是 MF 原本替我们做的 Annex B → 长度前缀转换与 `avcC`/`hvcC` 构建，
//!   现在由 [`nal`] 负责。
//!
//! GPU AV1 与 GPU HEVC 一同接入了自研封装（[`nal::build_av1c`] + `RawFragmented`），
//! 同样可生成分片 fMP4：具备抗崩溃能力且支持录制中随时无损提取切片。
//!
//! 区间切片（[`extract`]）只依赖「文件里有 `moof`」这一条件，与编码器无关：
//! 它照抄源文件的 `stsd`、整段复制样本字节、只平移 `co64` 偏移。所以软编、
//! GPU H.264、GPU HEVC、GPU AV1 录制中的文件随时可切。
//!
//! co64 已消除 32 位偏移上限、mdat 用 64 位 largesize，单段录制不再受 4 GiB 约束。

pub mod extract;
pub mod fragment;
pub mod mp4;
pub mod nal;
pub mod remux;
