//! 内蔵 whisper モデルの管理（初回ダウンロードと保存先の解決）。
//!
//! モデルはバイナリに埋め込まず（数百 MB の肥大化を避ける）、初回の文字起こし時に HTTPS で
//! 取得して OS 標準のデータディレクトリへ保存し、以後は再利用する（「内蔵」の実現方式）。
//! ユーザーはモデルを意識せず、設定のトグルを ON にするだけで文字起こしを使える。
//!
//! ダウンロードは既知の SHA-256 で検証し、一時ファイル（`.part`）→リネームで原子的に配置する
//! （破損・部分ダウンロードをモデルとして残さない）。通信は受信のみで、音声などの機微データは
//! 一切送信しない（`docs/CONTEXT.md` のオンデバイス方針はそのまま）。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// 内蔵モデルのファイル名。多言語の small（ggml 形式）。日本語会議の文字起こしが主用途のため、
/// 精度と負荷・ダウンロードサイズのバランスで small を選ぶ。
const MODEL_FILENAME: &str = "ggml-small.bin";

/// 内蔵モデルの取得元（whisper.cpp 公式の配布リポジトリ）。
const MODEL_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin";

/// `MODEL_URL` の公式 SHA-256（HuggingFace の LFS メタデータより）。改ざん・破損の検知に使う。
/// モデルを差し替えるときは URL とペアで必ず更新する。
const MODEL_SHA256: &str = "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b";

/// おおよそのダウンロードサイズ（MB）。ログでユーザーに待ち時間の目安を伝える用途のみ。
const MODEL_SIZE_MB: u64 = 465;

/// ダウンロード時の読み書きバッファサイズ。
const DOWNLOAD_BUF_SIZE: usize = 64 * 1024;

/// 内蔵モデルのパスを返す。未取得ならダウンロードして配置する（成功するまで返さない）。
///
/// 文字起こしワーカースレッドから呼ばれる想定（ダウンロードは分オーダーかかりうるため、
/// メインスレッドから呼ばない）。失敗してもアプリは落とさず、呼び出し側がログして
/// 当該ジョブをスキップする（次のジョブで再試行される）。
pub fn ensure_model() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = model_path().ok_or("Cannot determine the data directory")?;
    if path.is_file() {
        return Ok(path);
    }
    download_model(&path)?;
    Ok(path)
}

/// 内蔵モデルの保存先（`<データディレクトリ>/models/ggml-small.bin`）。
fn model_path() -> Option<PathBuf> {
    crate::config::data_dir().map(|dir| dir.join("models").join(MODEL_FILENAME))
}

/// モデルをダウンロードして `dest` へ原子的に配置する。
fn download_model(dest: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = dest.parent() {
        // モデルは公開配布物で機微データではないため、権限は OS 既定でよい
        // （録音データの 0700/0600 とは扱いが異なる）。
        std::fs::create_dir_all(parent)?;
    }
    println!(
        "Downloading the Whisper speech model (about {MODEL_SIZE_MB} MB). Transcription starts after the download completes"
    );

    let mut response = ureq::get(MODEL_URL).call()?;
    let reader = response.body_mut().as_reader();

    // 一時ファイルへ書き、検証に通ってから本来の名前へ rename する（原子的）。途中で失敗しても
    // 壊れた/部分的なファイルがモデルとして残らない。
    let part = dest.with_extension("bin.part");
    let result = write_verified(reader, &part, MODEL_SHA256);
    if let Err(err) = result {
        // 後始末の失敗も黙って捨てない（docs/rules/error-handling.md）。
        if let Err(remove_err) = std::fs::remove_file(&part)
            && remove_err.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("Failed to remove the partially downloaded model: {remove_err}");
        }
        return Err(err);
    }
    std::fs::rename(&part, dest)?;
    println!("Downloaded the Whisper speech model");
    Ok(())
}

/// `reader` の内容を `dest` へ書き出しつつ SHA-256 を計算し、`expected_sha256` と一致しなければ
/// エラーを返す（ファイルは書かれたまま残るため、後始末は呼び出し側で行う）。
fn write_verified(
    mut reader: impl Read,
    dest: &Path,
    expected_sha256: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::create(dest)?;
    let mut writer = std::io::BufWriter::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; DOWNLOAD_BUF_SIZE];
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        writer.write_all(&buf[..read])?;
    }
    writer.flush()?;

    let digest = format!("{:x}", hasher.finalize());
    if digest != expected_sha256 {
        return Err(format!("checksum mismatch (expected {expected_sha256}, got {digest})").into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 既知データの SHA-256（`echo -n hello | sha256sum` 相当）。
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("openshoki-model-{}-{name}", std::process::id()))
    }

    #[test]
    fn write_verified_accepts_matching_checksum() {
        let dest = temp_path("ok.bin");
        write_verified(b"hello".as_slice(), &dest, HELLO_SHA256)
            .expect("matching checksum should succeed");
        assert_eq!(std::fs::read(&dest).expect("readable"), b"hello");
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn write_verified_rejects_checksum_mismatch() {
        let dest = temp_path("bad.bin");
        let err = write_verified(b"tampered".as_slice(), &dest, HELLO_SHA256)
            .expect_err("checksum mismatch should fail");
        assert!(err.to_string().contains("checksum mismatch"));
        let _ = std::fs::remove_file(&dest);
    }

    /// 実ダウンロードのスモーク（約 465MB・要ネットワーク）。ローカルで
    /// `cargo test ensure_model -- --ignored` により実行する。取得済みなら即成功する
    /// （実アプリの初回文字起こしと同じ経路・同じ保存先）。
    #[test]
    #[ignore = "downloads ~465MB; run manually with --ignored"]
    fn ensure_model_downloads_and_verifies() {
        let path = ensure_model().expect("the model should download and verify");
        assert!(path.is_file());
    }
}
