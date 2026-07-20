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

/// 受信を打ち切るサイズ上限。既知のモデルサイズ＋十分な余裕。配信元の故障・想定外の応答で
/// ディスクを埋め尽くさないための保険（正常時は届かない）。
const MAX_DOWNLOAD_BYTES: u64 = 600 * 1024 * 1024;

/// 接続確立・応答ヘッダ受信のタイムアウト。
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// ボディ受信全体のタイムアウト。低速回線でも 465MB を受け切れる長さにしつつ、無応答の
/// 接続（half-open 等）でワーカーが恒久にハングしないようにする（ワーカーは 1 本の逐次処理
/// なので、ここで詰まると以後の文字起こしが全て止まる）。超過時は失敗し、次のジョブで再試行する。
const RECV_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

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

    // タイムアウトを明示する。ureq の既定は無期限で、無応答の接続（half-open 等）に当たると
    // 逐次処理のワーカーが恒久にハングし、以後の文字起こしが全て止まってしまう。
    // TLS 検証は ureq 既定（rustls + 同梱 Mozilla ルート）。OS のトラストストアとは独立だが、
    // 接続先は固定 1 URL で SHA-256 ピンによる完全性検証も重ねているため、これで足りる。
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(CONNECT_TIMEOUT))
        .timeout_recv_body(Some(RECV_BODY_TIMEOUT))
        .build()
        .into();
    let mut response = agent.get(MODEL_URL).call()?;
    let reader = response.body_mut().as_reader();

    // 一時ファイルへ書き、検証に通ってから本来の名前へ rename する（原子的）。途中で失敗しても
    // 壊れた/部分的なファイルがモデルとして残らない。一時ファイル名はプロセス固有にする:
    // アプリの多重起動（単一インスタンス化 #39 は未実装）で 2 プロセスが同名の一時ファイルへ
    // 同時に書くと、ハッシュは各自の受信ストリームで計算されるためファイルの破損を検知できず、
    // 壊れた内容が検証済みモデルとして配置されうる。名前を分ければ各自が自分の書いた内容だけを
    // 検証し、rename（原子的・後勝ち）はどちらも検証済みなので安全になる。
    let part = dest.with_extension(format!("part.{}", std::process::id()));
    let result = write_verified(reader, &part, MODEL_SHA256, MAX_DOWNLOAD_BYTES)
        .and_then(|()| std::fs::rename(&part, dest).map_err(Into::into));
    if let Err(err) = result {
        // 後始末の失敗も黙って捨てない（docs/rules/error-handling.md）。
        if let Err(remove_err) = std::fs::remove_file(&part)
            && remove_err.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("Failed to remove the partially downloaded model: {remove_err}");
        }
        return Err(err);
    }
    println!("Downloaded the Whisper speech model");
    Ok(())
}

/// `reader` の内容を `dest` へ書き出しつつ SHA-256 を計算し、`expected_sha256` と一致しなければ
/// エラーを返す（ファイルは書かれたまま残るため、後始末は呼び出し側で行う）。
/// `max_bytes` を超える受信は打ち切る（想定外の応答でディスクを埋めない保険。テスト容易性の
/// ため引数で受け、実運用は `MAX_DOWNLOAD_BYTES`）。
fn write_verified(
    mut reader: impl Read,
    dest: &Path,
    expected_sha256: &str,
    max_bytes: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::create(dest)?;
    let mut writer = std::io::BufWriter::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; DOWNLOAD_BUF_SIZE];
    let mut written: u64 = 0;
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        written += read as u64;
        if written > max_bytes {
            return Err(format!("download exceeded the size limit ({max_bytes} bytes)").into());
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
        write_verified(b"hello".as_slice(), &dest, HELLO_SHA256, MAX_DOWNLOAD_BYTES)
            .expect("matching checksum should succeed");
        assert_eq!(std::fs::read(&dest).expect("readable"), b"hello");
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn write_verified_rejects_checksum_mismatch_and_leaves_file() {
        let dest = temp_path("bad.bin");
        let err = write_verified(
            b"tampered".as_slice(),
            &dest,
            HELLO_SHA256,
            MAX_DOWNLOAD_BYTES,
        )
        .expect_err("checksum mismatch should fail");
        assert!(err.to_string().contains("checksum mismatch"));
        // doc の契約どおり、失敗してもファイルは残る（後始末は呼び出し側の責務）。
        assert!(dest.is_file(), "the partial file should remain for cleanup");
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn write_verified_handles_multi_chunk_input() {
        // 読み書きループが複数チャンクにまたがる経路（部分バッファの逐次 hash/write）を通す。
        let data = vec![0xA5u8; DOWNLOAD_BUF_SIZE * 2 + 123];
        let expected = format!("{:x}", Sha256::digest(&data));
        let dest = temp_path("multi.bin");
        write_verified(data.as_slice(), &dest, &expected, MAX_DOWNLOAD_BYTES)
            .expect("matching checksum should succeed");
        assert_eq!(
            std::fs::metadata(&dest).expect("metadata").len(),
            data.len() as u64
        );
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn write_verified_aborts_over_size_limit() {
        // 上限超過の入力は途中で打ち切る。実運用の 600MB を流すと重いので、小さな上限で
        // 打ち切り経路そのものを検証する（上限は引数化されており同じコードパス）。
        let dest = temp_path("oversize.bin");
        let limit = DOWNLOAD_BUF_SIZE as u64;
        let err = write_verified(
            std::io::Read::take(std::io::repeat(0), limit + DOWNLOAD_BUF_SIZE as u64),
            &dest,
            HELLO_SHA256,
            limit,
        )
        .expect_err("exceeding the size limit should fail");
        assert!(err.to_string().contains("size limit"));
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
