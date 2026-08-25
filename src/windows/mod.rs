//! ウィンドウごとの配線。`main.rs` が肥大しないよう、画面単位で切り出す。
//!
//! 機能ごとのウィンドウ（#141）と、それらが共有するモデル一覧（#140）。設定画面・Library の
//! 配線は `main.rs` に残っている。

pub(crate) mod minutes;
pub(crate) mod models;
pub(crate) mod transcription;
