//! 重い ML 推論（whisper の文字起こしと、議事録要約の LLM）を**同時に 1 本しか走らせない**ための
//! 共有スロット。
//!
//! 文字起こしと要約は別々の逐次ワーカー（`src/transcribe.rs` / `src/summarize.rs`）で動く。
//! 同一セッションでは「文字起こし成功 → 要約投入」の順なので重ならないが、**別セッションの
//! ジョブがキューにあると重なる**（連続した会議、Recordings ウィンドウからの手動再実行など）。
//! 重なると常駐アプリのピークが加算され、要約 7B のピーク RSS 約 8.2GB に文字起こしの
//! GB 級 PCM バッファが乗る。メモリ逼迫は録音中のプロセスごと落としうるので、ここで直列化する。
//!
//! スロットを取るのは**モデルを実際に動かす区間だけ**にする。モデルのダウンロード（数 GB・
//! 分オーダー）は CPU もメモリも食わないので、待たせても誰の得にもならない。
//!
//! 後処理（正規化・ミックス生成。`src/mixdown.rs`）はここに含めない。ML ではなく、
//! 音源 1 本ぶんのデコード／再エンコードで完結するため。

use std::sync::{Arc, Mutex, MutexGuard};

/// 重い推論の実行権。`Clone` で各ワーカーへ配り、同じ 1 枠を取り合わせる。
#[derive(Clone, Default)]
pub struct InferenceSlot {
    /// 実行権そのもの。値は持たず、ロックの保持＝占有を表す。
    held: Arc<Mutex<()>>,
}

impl InferenceSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// 空くまで待って占有する。**戻り値を保持している間だけ**占有が続くので、
    /// `let _slot = slot.acquire();` のように束縛すること（`let _ = ...` は即座に手放す）。
    ///
    /// poison（占有中のパニック）でも推論を止めないため、ガードを取り出して続行する
    /// （`docs/rules/error-handling.md`）。保護している値が無いので、壊れた状態を引き継ぐ心配は無い。
    pub fn acquire(&self) -> MutexGuard<'_, ()> {
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// クローンした 2 つのハンドルが同じ枠を取り合う（＝一方が占有中は他方が待つ）こと。
    /// 待ちを直接観測すると sleep 依存になるので、`try` 相当の代わりに「占有を手放したあとは
    /// 取れる」ことと「別スレッドが取れるまで進まない」ことを、完了順で確かめる。
    #[test]
    fn clones_share_one_slot() {
        let slot = InferenceSlot::new();
        let other = slot.clone();
        let order = Arc::new(Mutex::new(Vec::new()));

        let held = slot.acquire();
        let worker_order = Arc::clone(&order);
        let worker = std::thread::spawn(move || {
            let _slot = other.acquire();
            worker_order.lock().expect("not poisoned").push("worker");
        });

        // 占有中は worker が先へ進めない。ここで記録した "main" は必ず先に入る。
        std::thread::sleep(std::time::Duration::from_millis(50));
        order.lock().expect("not poisoned").push("main");
        drop(held);

        worker.join().expect("the worker thread should finish");
        assert_eq!(*order.lock().expect("not poisoned"), vec!["main", "worker"]);
    }
}
