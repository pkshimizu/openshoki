# アプリの多重起動防止

- 作成日: 2026-07-14
- ステータス: ドラフト

## 概要

openshoki が同時に複数プロセス起動しないようにする。常駐（メニューバー）アプリのため
2 つ以上動くとトレイアイコンが重複し、録音の自動開始（マイク使用検知・アプリ再生連動）が
複数プロセスで同時発火して同じ音を二重録音する・保存先を奪い合うなどの不整合を招く。
起動時にロックファイルで排他を取り、既に動作中なら英語ログを出して即終了する。

## 背景・前提（コンテキスト）

- openshoki は Slint + tray-icon による**常駐**型のデスクトップ録音アプリ。起動時は
  ウィンドウを出さずトレイに常駐する（`src/main.rs`、`docs/CONTEXT.md`）。
- macOS `.app` を Finder / LaunchServices 経由で開いた場合はバンドル ID による重複起動抑止が
  効くが、`cargo run` / `cargo dev`（`cargo-watch` によるホットリロード）や `open` での
  重複コピー起動、複数ターミナルからの起動では抑止されない。開発・配布の双方で明示的な
  ガードに価値がある。
- 設定・パス取得は既に `directories`（`ProjectDirs`）を使用している（`src/config.rs`）。
  識別子は `QUALIFIER="net"` / `ORGANIZATION="noncore"` / `APPLICATION="openshoki"`。
- エラー時はアプリを落とさずログへ（`docs/rules/error-handling.md`）が基本方針。ただし本件は
  「そもそも起動を続けさせない」のが目的なので、多重起動検知時は例外的に**意図した終了**を行う。
- ユーザー／ターミナルに見せる文言は英語（`docs/CONTEXT.md`「ユーザーに見せるメッセージは英語」）。

## 要件

- 2 つ目以降のプロセスが起動しても、録音・トレイ常駐を始めずに終了する。
- 2 つ目の起動時は「既に動作中である」旨を英語ログ（`eprintln!`）で出してから終了する。
- ガードは全 OS 共通で効くよう OS 非依存に実装する（macOS/Windows/Linux）。将来の
  Windows/Linux 展開でもそのまま働く。
- 検知は `fs2` クレートのロックファイル（`try_lock_exclusive`）で行う。プロセス終了・
  クラッシュ時に OS が自動でロックを解放するため、stale lock（前回の残骸で起動不能）に
  ならないこと。
- 正常なプロセス（1 つ目）の起動・常駐・終了の挙動は従来どおり変えない。

- スコープ外:
  - 2 つ目の起動から 1 つ目へ通知し、既存の設定ウィンドウを前面に出すこと（IPC が必要。
    別 issue で検討）。
  - LaunchServices / `.app` バンドル側での重複起動抑止設定の変更。

## 確定した論点

ユーザーへの確認で以下を確定（いずれも推奨案を採用）。

- **多重起動時の振る舞い = ログを出して終了**: 2 つ目は英語ログを出して即終了する。
  常駐アプリでは 2 つ目から既存トレイを前面に出す手段が自明でなく、IPC を伴うため、
  まずは単純で確実な「終了」に倒す。理由の見える化のためログは出す（無言終了にしない）。
- **対象 OS = 全 OS 共通**: ロックファイル方式は OS 非依存で書けるため `cfg` で囲まず共通実装
  にする。CONTEXT の「macOS 先行」は録音・システム音声など OS API 依存機能の話で、本件は
  該当しない。
- **検知方式 = fs2 でロックファイル**: `fs2` の `FileExt::try_lock_exclusive()` を使う。
  flock ベースでプロセス終了・クラッシュ時に自動解放されるため stale lock が起きず、安全な
  Rust API で完結し `docs/rules/ffi.md` が対象とする `unsafe` FFI を避けられる。自前 flock
  （FFI）や single-instance クレートより本プロジェクトの最小依存・非 unsafe 方針に合う。

## 実装方針

- 新規モジュール `src/single_instance.rs` に排他取得のロジックをまとめる。
  - ロックファイルのパスは `ProjectDirs::from("net","noncore","openshoki")` の
    `cache_dir()` 配下に固定名（例 `openshoki.lock`）で置く。`config.rs` と同じ識別子を使う。
    ディレクトリが無ければ `create_dir_all` で作る。`ProjectDirs` 取得不可の異例環境では
    ガードを諦め（ログを出して）起動は続行する（ガードのために起動不能にはしない）。
  - `File` を open（作成）し `fs2::FileExt::try_lock_exclusive()` を試みる。
    - 成功: 取得した `File` を保持したガード（`InstanceGuard`）を返す。**ロックは `File` が
      生きている限り保持されるため、ガードを `main` の最後まで生存させる**（drop でロック解放）。
    - 失敗（`WouldBlock` 相当 = 既に他プロセスが保持）: 「別インスタンスが動作中」と判断し、
      呼び出し側へ「多重起動」を表すシグナルを返す。
  - ロックファイルの中身（PID 等）は排他自体には不要。当面は空でよい（診断用に PID を書くのは
    任意・後続）。
- `main.rs` の**最初**（`hide_dock_icon()` や `AppWindow::new()` より前、Slint / トレイ初期化
  より前）で排他を取得する。多重起動なら英語ログを出して `return Ok(())`（正常終了扱い）。
  ここで早期 return することで、トレイ・ウィンドウ・モニタを一切初期化しない。
- 取得した `InstanceGuard` は `main` のローカル変数として関数末尾まで保持する（`_guard` などで
  束縛し、早期 drop されないようにする）。`slint::run_event_loop_until_quit()` の後、`main` を
  抜ける際に自然に drop されロックが解放される。
- `Cargo.toml` に `fs2` を追加する（`[dependencies]`、通常の cross-platform 依存）。

## 実装ステップ

1. `Cargo.toml` の `[dependencies]` に `fs2`（最新の 0.4 系）を追加し、`cargo build` が通ることを
   確認する。
2. `src/single_instance.rs` を新規作成する。
   - ロックファイルパス算出（`ProjectDirs` の `cache_dir()` + `openshoki.lock`、親ディレクトリ
     作成）を実装。
   - `acquire()`（仮）を実装: `File` open → `try_lock_exclusive()`。成功で `InstanceGuard`
     を返し、既ロックなら「多重起動」を表す結果を返す。`ProjectDirs` 取得不可・IO 失敗時は
     ガード無しで起動続行する分岐（英語ログ）を用意する。
   - `InstanceGuard` はロック済み `File` を保持する型にし、doc コメントで「生存中はロック保持、
     drop で解放」を明記する。文言・ログは英語（`docs/CONTEXT.md`）。
3. `src/main.rs` に `mod single_instance;` を追加し、`main` 冒頭で `acquire()` を呼ぶ。
   - 多重起動なら英語ログ（例: `Another instance is already running; exiting.`）を出して
     `return Ok(())`。
   - 取得できたガードを `let _guard = ...;` で関数末尾まで保持する。
4. 動作確認（`docs/rules/`・手動検証）:
   - 1 つ目: `cargo run` で従来どおりトレイ常駐・録音・終了できる。
   - 2 つ目: 1 つ目を起動したまま別ターミナルで `cargo run` すると、英語ログを出して即終了し、
     トレイアイコンが増えない。
   - 1 つ目を終了（Quit）した後に再度 `cargo run` すると、今度は正常に起動する（ロックが
     解放され stale にならない）。強制終了（kill -9）後も同様に再起動できることを確認する。
5. 既存テスト（`cargo test`）が通ることを確認する。必要なら `single_instance` のパス算出等の
   純粋部分に単体テストを足す（ロック取得自体はプロセス/FS 依存のため結合検証で担保）。

## 影響範囲・リスク

- 影響を受けるファイル/モジュール:
  - `Cargo.toml`（`fs2` 追加）
  - `src/main.rs`（`mod single_instance;`、`main` 冒頭のガード、`_guard` 保持）
  - `src/single_instance.rs`（新規）
- リスクと対策:
  - **ガード変数の早期 drop**: `acquire()` の戻り値を `let _ = ...` で捨てるとロックが即解放され
    多重起動を防げない。名前付き（`let _guard`）で末尾まで束縛し、doc コメントで生存要件を明記。
  - **ロックファイルの置き場所**: `cache_dir` はクリーンアップされうるが、ファイルが消えても
    flock の意味論上は「動作中プロセスが握るロック」が真実なので、削除されても実害は
    起きにくい（次回作り直すだけ）。runtime dir は macOS/Windows で None のため cache_dir を採用。
  - **`ProjectDirs` 取得不可・IO エラー**: ガードを取れない異例環境ではガード無しで起動続行し
    （英語ログ）、ガードのために起動不能にはしない（`docs/rules/error-handling.md` の精神）。
  - **`cargo dev`（cargo-watch）再起動時の競合**: 旧プロセスの終了と新プロセスの起動が一瞬
    重なるとロックが取れず新プロセスが即終了しうる。実害は次の保存で再起動される程度だが、
    検証時に挙動を確認する。問題が目立つ場合はリトライ（短い待機→再取得）を後続で検討。

## 未確定事項

- ロックファイルに PID を書いて診断性を上げるか（当面は空で可、任意の後続）。
- `cargo dev` の高速再起動での取得失敗にリトライを入れるか（検証結果を見て判断）。
- 2 つ目の起動から 1 つ目の設定ウィンドウを前面化する IPC は別 issue とする。
