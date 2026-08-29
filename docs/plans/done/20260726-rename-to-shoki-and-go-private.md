# アプリ名を shoki に変更し、リポジトリを private にする

- 作成日: 2026-07-26
- ステータス: ドラフト

## 概要

アプリ名を `openshoki` から `shoki` へ変更し、GitHub リポジトリ `pkshimizu/openshoki` を
`pkshimizu/shoki` へリネームしたうえで private にする。公開しない個人用ツールという実態に
対して「open」がそぐわないため。名前はクレート名・bundle id・設定パス・アイコン資産名まで
波及するので一括で入れ替える。あわせて、private 化によって有料になる CI をセルフホスト
ランナーへ移し、課金を発生させない。

## 背景・前提（コンテキスト）

調査で確認した現状:

- リポジトリは **public**、Releases なし、star / fork なし、license なし。issue 64 件・PR 46 件。
- アプリ識別子は `src/config.rs` の `QUALIFIER = "net"` / `ORGANIZATION = "noncore"` /
  `APPLICATION = "openshoki"`。設定の実体は
  `~/Library/Application Support/net.noncore.openshoki/config.toml`。同ファイルのコメントは
  **「一度決めたら変えない（変えると過去の設定ファイルを見失う）」**と明記している。今回は
  それを承知のうえで変える（対策は「実装ステップ 2」）。
- 既定の録音保存先フォルダ名は `DEFAULT_DIR_NAME = "openshoki"`（Documents 配下）。
- MAS 検証（[#77](https://github.com/pkshimizu/openshoki/issues/77)）で
  `net.noncore.openshoki.masprobe` という bundle id 体系を使っている。
- アイコンは `assets/icon/openshoki.icon` がマスターで、`scripts/generate-icons.sh` が
  `--app-icon openshoki` から `assets/icon/generated/openshoki.icns` を作る。
  `scripts/check-icons.sh` と CI が「コミット済み生成物とマスターの一致」を検査する。
- CI（`.github/workflows/ci.yml`）は 2 ジョブ構成。`audit`（ubuntu-latest / cargo audit）と
  `check`（macos-latest / fmt・clippy・build・test・アイコン一致検査）。トリガーは main への
  push と全 PR、同一 ref は `cancel-in-progress`。
- 直近 30 日の CI は **145 run / 実時間 428 分**。public では無料だが、private では
  無料枠 2,000 分/月に対して **macOS ランナーは 10 倍消費**するため、換算 3,000 分超となり
  枠を確実に超える。
- `docs/` は `.gitignore` されており、`docs/CONTEXT.md` やこのプランは **git 管理外**（ローカルのみ）。
- 旧名の出現は **21 ファイル・86 箇所**（`target/` と `.git/` を除く）。

## 要件

- アプリ名を `shoki` にする。クレート名・bundle id・設定パス・既定保存先・アイコン資産名・
  表示文字列・スレッド名・テストの一時ディレクトリ名・環境変数まで一貫させる。
- GitHub リポジトリを `pkshimizu/shoki` へリネームし、visibility を private にする。
- private 化後の CI をセルフホストランナー（手元の Mac）で回し、Actions の課金をゼロにする。
- スコープ外:
  - 既存の設定・録音データの**自動**移行（コードは書かず、手で移す手順だけ用意する）
  - 配布まわりの作業そのもの（[#20](https://github.com/pkshimizu/openshoki/issues/20) の
    `.app` バンドル、[#77](https://github.com/pkshimizu/openshoki/issues/77) の MAS）
  - git 履歴・過去の issue / PR タイトルに残る旧名の書き換え
  - `audit` ジョブのセルフホスト化（ubuntu は 1 倍消費で月 150 分程度。無料枠に収まる）

## 確定した論点

いずれもユーザーへの確認で決定した。

- **新しい名前は `shoki`（書記）**: `openshoki` から `open` を落とすだけなので、「録音を書記する」
  というアプリの中核の意味と、README・CONTEXT.md の説明文がそのまま使える。bundle id は
  `net.noncore.shoki` になる。候補にあった `sumi`（新アイコンの和紙×墨と一致）・`ippitsu`・
  `hisho` は、リネームの手間が同じなら意味の継続性を優先して見送った。
- **リポジトリ名も `shoki` へ変更する**: アプリ名と揃える。GitHub は旧 URL を新 URL へ
  リダイレクトするので、README 内の issue リンク 8 本は壊れない（それでも表記は揃える）。
- **既存データは手で移す（コード変更なし）**: 使うのは開発者本人だけなので、移行コードを
  永久にコードベースへ抱えるより、1 回の手作業で済ませる。旧フォルダは残るため失敗しても
  破壊的ではない。
- **CI はセルフホストランナーへ切り替える**: 上記の試算どおり private では無料枠を超えるため。
  手元 Mac で回せば課金ゼロで、Xcode 26 が要るアイコン検査の環境も安定する。private リポジトリ
  なので、self-hosted ランナー特有のセキュリティ懸念（fork PR による任意コード実行）も小さい。

## 実装方針

- **一括置換はしない**。同じ `openshoki` でも、識別子（設定パス・bundle id）／クレート名／
  表示文字列／資産名で影響範囲と検証方法が違う。意味ごとに分けて置き換え、それぞれの完了条件で
  確認する。特に `Cargo.toml` の `name` 変更はバイナリ名とアイコン資産名に波及する。
- **順序は「コード → リポジトリ名 → private 化 → ランナー切替」**とする。理由:
  - コードのリネームは public のまま（=CI 無料）で検証を終えられる。
  - 逆に「public のまま self-hosted を有効化」する順序は採らない。fork からの PR が手元の Mac で
    任意コードを実行できてしまうため。private 化を先に済ませてからランナーを向ける。
  - この順序だと、ランナー切替 PR の CI が 1 回だけ macos-latest で走って 30 分程度課金されるが、
    無料枠に十分収まる。
- アイコンは名前変更後に `scripts/generate-icons.sh` を再実行し、生成物をコミットする。
  CI の一致検査が通ることを完了条件にする（再生成忘れは CI で落ちる設計になっている）。

## 実装ステップ

### 1. コードと資産のリネーム（ブランチ 1 本）

置き換える対象を意味ごとに分ける。

1. **クレート名**: `Cargo.toml` の `name`、`Cargo.lock`
2. **識別子とパス**: `src/config.rs` の `APPLICATION` / `DEFAULT_DIR_NAME`、および同ファイルの
   テスト内パス文字列（`/tmp/openshoki-*`）
3. **表示文字列**: `src/tray.rs` のツールチップ 2 種、`ui/app-window.slint` の `title` と
   `"openshoki Settings"`、`src/main.rs` の多重起動メッセージ
4. **内部名**: スレッド名（`src/recorder.rs` / `src/system_audio.rs`）、テストの一時ディレクトリ名
   （`player` / `recordings` / `transcript` / `mixdown` / `whisper_model` / `transcribe` /
   `single_instance`）、コメント中の言及
5. **環境変数**: `OPENSHOKI_WHISPER_MODEL` → `SHOKI_WHISPER_MODEL`（`src/transcribe.rs` の
   `#[ignore]` テストとその doc コメント）
6. **アイコン資産**: `assets/icon/openshoki.icon` → `assets/icon/shoki.icon`、
   `scripts/generate-icons.sh`（`--app-icon shoki`・パス・コメント）、`scripts/check-icons.sh`、
   `.gitignore` のコメント。再生成して `generated/shoki.icns` をコミット（旧 `openshoki.icns` は削除）
7. **MAS プローブ**: `scripts/mas-probe.sh` の `bundle_id` / `app_name` / Info.plist の説明文、
   `examples/mas_probe.rs`
8. **ドキュメント**: `README.md`、`docs/CONTEXT.md`（git 管理外だが同期させる）

**完了条件**:

- 旧名の残存が**想定内のものだけ**であること。このリポジトリの `grep` は gitignore された
  パス（`docs/` 配下すべて）を黙ってスキップするため、2 本立てで見る:
  `git grep -i openshoki`（追跡ファイル）と
  `/usr/bin/grep -rn -i openshoki --exclude-dir=target --exclude-dir=.git .`（全体）。
  想定内として残るのは次の 3 つだけで、これ以外が出たら取りこぼし:
  - `README.md` のリポジトリ URL（7 行・8 リンク）— リポジトリ名変更は手順 3 で行うため、
    先に書き換えると 404 になる
  - `src/config.rs` と `docs/rules/messages.md` の改名の経緯（旧名への言及）
  - `docs/plans/done/` の過去プラン — 完了済みの記録なので当時の名前のまま
- `./scripts/generate-icons.sh` 実行後に `./scripts/check-icons.sh` が通る
- `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test` / `cargo build` が通る
- `cargo run` で起動し、トレイのツールチップと設定画面のタイトルが新名になっている

### 2. 手元データの移し替え（1 回だけの手作業）

手順 1 をマージしたあと、アプリを**終了してから**行う。**素の `mv` 2 本ではいけない**:
宛先が既に存在すると `mv` は入れ子に移動して exit 0 で成功に見え、設定は全喪失、旧録音は
ログも出ずに一覧から消える（新バイナリを一度でも起動すると宛先が作られる）。

安全な手順は #111 の PR（#113）本文に置いた。要点は次のとおり:

- 先に常駐アプリを終了し、旧バイナリ（`target/{debug,release}/openshoki`）を消す
  （多重起動ロックは識別子ごとに別物なので、旧・新が同時に起動できてしまう）
- 宛先の非存在を確かめてから `mv`。`set -e` と `exit 1` を対話シェルに残さないよう
  サブシェルで囲む
- `recording_dir`（絶対パス）を `sed` で書き換える。忘れると旧パスが無言で再作成され、
  移行済みの録音が一覧から消える
- 起動して設定・録音一覧・手動録音を確認してから、旧フォルダを掃除する
- コードブロックにインラインの `#` コメントを置かない（zsh は `interactive_comments` が
  既定 off で、貼り付けると `#` 以降が引数として渡る）

### 3. リポジトリ名の変更と private 化

```sh
gh repo rename shoki --repo pkshimizu/openshoki
git remote set-url origin git@github.com:pkshimizu/shoki.git
gh repo edit pkshimizu/shoki --visibility private --accept-visibility-change-consequences
```

**完了条件**: `gh repo view --json name,visibility` が `shoki` / `PRIVATE` を返す。
`git fetch` と `gh issue list` が新 URL で通る。

### 4. セルフホストランナーの用意と CI の切替

1. 登録トークンを取得してランナーを登録する（Settings → Actions → Runners からでもよい）。

   ```sh
   gh api -X POST repos/pkshimizu/shoki/actions/runners/registration-token -q .token
   ```

2. ランナーを launchd 常駐にする（Mac の再起動後も上がるように）。

   ```sh
   ./svc.sh install && ./svc.sh start
   ```

3. `.github/workflows/ci.yml` の `check` ジョブを
   `runs-on: [self-hosted, macOS, ARM64]` に変更する。`audit` ジョブは `ubuntu-latest` のまま。
4. ランナー機に必要なツールが揃っていることを確認する: **Xcode 26 以降**（`.icon` を扱う
   `actool`）、`cmake`（whisper.cpp）、`rsvg-convert` / `magick`（アイコン検査）、Rust ツールチェイン。

**完了条件**: PR を 1 本出して self-hosted 上で全ステップが緑になる。以降、Actions の
使用量（Billing）が増えない。

### 5. 後片付け

- `README.md` の GitHub リンク 8 本を新 URL へ更新する（リダイレクトは効くが表記を揃える）。
- [#20](https://github.com/pkshimizu/openshoki/issues/20) と
  [#77](https://github.com/pkshimizu/openshoki/issues/77) に、bundle id が `net.noncore.shoki`
  になった旨と、下記「配布ができなくなる」リスクをコメントで残す。
- `docs/CONTEXT.md` に新しい前提（アプリ名・識別子・CI がセルフホストランナーであること・
  ランナー機に必要なツール）を反映する。

## 影響範囲・リスク

**影響を受けるファイル**: 21 ファイル・86 箇所。内訳は `Cargo.toml` / `Cargo.lock`、
`src/` の 13 ファイル（`config.rs` が 17 箇所で最多）、`ui/app-window.slint`、
`scripts/` の 3 本、`examples/mas_probe.rs`、`README.md`、`.gitignore`、`assets/icon/` 配下。

**リスクと対策**:

- **設定・録音を見失う**: 手順 2 は手作業なので、忘れるとアプリが初期状態で立ち上がる。
  ただし旧フォルダは残るため破壊は起きない。気づいた時点で移せばよい。実装 PR の説明に
  手順をそのまま書いておく。
- **アイコン生成物の取り残し**: `check-icons.sh` と CI が一致を検査するので、再生成忘れは
  CI で落ちる。手動確認に頼らない。
- **将来の一般配布が塞がる**（**2026-07-26 に方針決定。下記「配布方針」参照**）:
  private リポジトリの Releases は公開できないため、#20 の `.app` を
  GitHub Releases で配る道と、`pkshimizu/homebrew-tap` からの導線が使えなくなる。同じ理由で
  **GitHub Pages も塞がる**（private リポジトリの Pages は有料プラン限定）ため、LP（#75/#76）も
  このリポジトリでは公開できない。当初は「配布する段になったら
  「公開用の別リポジトリを作る」「MAS（#77）に寄せる」「直接配布」のいずれかを選ぶ
  判断が必要になる。今回 private を選んだ以上、これは後続で決める前提として残す。
- **ランナーの可用性**: Mac がスリープ／電源オフだと CI がキューで待ち続ける。急ぐときは
  `runs-on` を `macos-latest` に一時的に戻せる（その分は課金される）。
- **ランナーの環境ドリフト**: hosted と違って環境が使い回されるため、「手元にだけあるツール」に
  依存した CI が通ってしまうことがありうる。ランナー機のセットアップ手順を README に残し、
  必要なツールを明示する。
- **旧名の残骸**: git 履歴・過去の issue / PR タイトルは書き換えない。検索でヒットしても実害はない。

## 未確定事項

- セルフホストランナーを常駐させる Mac（現在の開発機と同一にするか）と、スリープ時の扱い。
- （解決済み）一般配布の方針 → 下記「配布方針」。
- `target/debug/openshoki` などバイナリ名に依存した手元のスクリプト・ランチャがあれば、
  あわせて更新が要る（リポジトリ内には見当たらなかったが、手元の設定までは追えていない）。

## 配布方針（2026-07-26 決定）

private 化で塞がるもの（Releases・Pages）を踏まえ、次のとおり決めた。

- **配布は Mac App Store に寄せる**（#109）。GitHub Releases での一般配布はしない。
- **#20 の Releases 添付はスコープから外す**。`.app` バンドルを組む部分は MAS
  パッケージング（#109）の土台として必要なので残す。
- **LP（#75 / #76）は専用の public リポジトリを別途作って公開する**。このリポジトリを private に
  したあとも LP を出せるようにするため。ダウンロード導線は GitHub Releases ではなく
  App Store へ向ける（MAS 公開前は「ソースからビルド」の案内に留める）。

この決定により、#112 の「補足: private 化で塞がること（後続の判断）」は解決済みになる。
