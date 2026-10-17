# pengitto

Linux 向けの Git クライアントです（仮称）。Fork の画面構成と操作感を参考に、Rust と egui で作っています。

## ビルドと起動

```bash
cargo build --release
./target/release/pengitto /path/to/repo   # リポジトリを指定して開く
./target/release/pengitto                 # 前回開いていたリポジトリを開く
```

- 必要なもの: Rust 1.95 以上、git、C リンカ（gcc）、fontconfig（`fc-match`）
- 開発用ヘッダ（GTK や Qt）は不要です。Wayland/X11 と Vulkan/GL は実行時に読み込みます。
- `pengitto --help` でオプション、`pengitto --self-check` で git と pengitto 自身の helper（askpass、rebase のエディタ）が動くかを確かめられます。

## インストール

**ホームに入れる**（`~/.local/bin/pengitto` と、メニュー・アイコン・ライセンスの一覧）

```bash
scripts/install.sh
```

- ビルドしてから入れます。cargo のない環境（ホストなど）では、ビルド済みの `target/release/pengitto` を `--no-build` で入れます。
- 入れる前に、入れる実行ファイルで `--self-check` を実行します。通らなければ（git がない・古いなど）、何も置かずに理由を表示します。
- 入れたものの記録（sha256 付き）を `~/.local/share/pengitto/install-manifest` に残します。pengitto のインストールが置いたものでないファイルや、その後に変えたファイルは上書きしません（`--force` で上書き）。ファイルを置く場所にフォルダーがある場合は、`--force` でも置きません。
- `--prefix DIR` で `DIR/bin` と `DIR/share` に入れられます。その `share` をメニューが見ない場所の場合は、そう表示します。
- 外すには `scripts/uninstall.sh` を実行します。設定と最近使った一覧（`~/.local/share/pengitto/` の `state.ron`・`app.ron`）は残します。入れた後に変えたファイルも残し、記録にも残すので、後から `scripts/uninstall.sh --force` で外せます。
- pengitto を実行中に入れ直しても、外しても、動いている pengitto は自分自身を helper として使い続けます。

**AppImage**（`target/appimage/pengitto-<版>-x86_64.AppImage`）

```bash
APPIMAGETOOL=/path/to/appimagetool APPIMAGE_RUNTIME=/path/to/runtime-x86_64 scripts/appimage.sh
```

- appimagetool と runtime は版を固定しています（`packaging/appimage/tools.lock` の sha256 と一致するものだけを使います）。
- ライブラリは同梱しません。動かすには、glibc 2.39 以上、Wayland か X11 と GPU（Vulkan か EGL）のライブラリ、git 2.43 以上、fontconfig が必要です。FUSE がない場合は `--appimage-extract-and-run` を付けて起動します（このとき展開した中身（約 35MB）が `/tmp` に残ります。版ごとに 1 つで、再起動すると消えます）。
- AppImage をそのまま起動しただけでは、Wayland のタスクバーのアイコンは汎用のものになります。メニューとアイコンを付けるには `scripts/install.sh --binary pengitto-….AppImage` で登録します。
- AppImage には runtime（type2-runtime、静的にリンクした libfuse（LGPL-2.1）など）が入っています。その通知は `packaging/APPIMAGE_NOTICES` にあります。他の人に配布する場合は、LGPL に従い runtime の対応するソースを示す必要があります。

**RPM**（Fedora 向け。`target/rpm/`）

```bash
scripts/rpm.sh            # HEAD から
scripts/rpm.sh --dirty    # コミットしていない変更（追跡しているファイル）も含める
```

- 依存するクレートを vendor してソースに入れるので、ビルドの間はネットワークを使いません。ソース RPM（`.src.rpm`）だけからも作り直せます（版は spec に書き込んであります）。
- `sudo dnf install ./target/rpm/pengitto-*.x86_64.rpm` で入れます。

**二つ起動した場合**: 動いている pengitto が、渡したリポジトリを開きます（同じデスクトップの中で 1 つ）。
- 2 つ目は、動いている pengitto が依頼を受け取った時点で終わります。開くのは、動いている側の画面が動いたときです（最小化中でも開きます。画面が固まっている間に渡したものは、戻ってから開きます）。
- メニューや Dolphin から起動すると、その起動のトークンで、動いているウィンドウを前に出すよう頼みます（Wayland の xdg-activation）。実際に前に出すかどうかは、デスクトップ（compositor）が決めます。端末から起動した場合（トークンがない場合）や X11 では、タスクバーで知らせるか、前に出すよう頼むだけです。
- X11 では、起動中の表示（砂時計など）を pengitto からは終わらせません。しばらくすると消えます。
- 別のウィンドウで動かしたいときは `--new-instance` を付けます。
- 動いている pengitto が応答しない場合は、新しいウィンドウを開き、そのことを知らせます（設定は、後から保存した方の内容になります。ファイルは壊れません）。
- 終了の確認中や、操作の停止を待っている間に起動すると、終了を取り止めて開きます。
- 更新した後は、一度閉じてから起動し直してください（動いている古い版が開くため）。

<!-- legacy-names:begin -->
**以前の名前（Tine）からの移行**（0.2.0 で pengitto に名前を変えました）
- 設定と最近使ったリポジトリは、初めて起動したときに `~/.local/share/tine/` から引き継ぎます（元のファイルは変えません）。
- `scripts/install.sh` で入れていた場合は、pengitto を同じ場所に入れると、Tine が入れたファイルを片付けます。入れた後に変えたファイルは残し、その一覧を表示します。
- RPM は、pengitto の RPM が tine を置き換えます。配布の形を変える場合（例: Tine は RPM、pengitto は AppImage）は、`sudo dnf remove tine` で外してください。
- Tine で始めて止まっているインタラクティブ rebase は、pengitto で開くと引き継ぎます（計画したメッセージと、退避した変更もそのまま使えます）。Tine を閉じてから開いてください。
<!-- legacy-names:end -->

## ライセンス

- pengitto 自体は MIT と Apache-2.0 のデュアルライセンスです（`LICENSE-MIT`、`LICENSE-APACHE`。どちらかを選んで使えます）。著作権者は K2Y です。
- pengitto に組み込んでいるクレートと素材（egui の既定フォント、syntect の構文定義）のライセンスは、`packaging/THIRD_PARTY_LICENSES` と About（☰ メニュー）で見られます。`scripts/third-party.py` が、x86_64 Linux のビルドの依存（`cargo tree`）と各クレートのソースから作ります。

## 操作

| キー | 動作 |
|---|---|
| Ctrl+O | リポジトリを開く |
| F5 / Ctrl+R | 再読込 |
| ↑ ↓ PageUp PageDown Home End | コミット一覧の移動 |
| Ctrl+W | タブを閉じる |
| Ctrl+Enter | コミット（コミットメッセージ入力中） |
| ダブルクリック | ファイルを stage / unstage |
| Ctrl+クリック / Shift+クリック | ファイルを複数選択 |
| 差分の行をクリック / ドラッグ | 行を選択（Ctrl でトグル、Shift で範囲） |
| F12 | デバッグ表示（フレーム時間・メモリ） |

## 動作上の注意

**commit-graph キャッシュの自動作成**
- 開いたリポジトリに git の commit-graph キャッシュがない場合、バックグラウンドで `git commit-graph write --reachable` を実行します。
- これは `git gc` や `git maintenance` が通常作成するものと同じファイルです。
- キャッシュがないと、大きなリポジトリでは履歴の最初の行が出るまで数秒かかります。

**変更の破棄**
- 破棄する前に確認ダイアログを出します。
- 追跡中のファイルの変更は元に戻せません。
- 未追跡ファイルは削除せず、デスクトップのゴミ箱に移します。

**hunk・行単位の操作**
- 差分の hunk 見出しにあるボタンで、hunk 全体または選択した行を stage / unstage / 破棄できます。破棄は確認してから実行します。
- 対象は、変更されたテキストファイルだけです。
- 新規・削除・名前変更・バイナリ・サブモジュール・モード変更のファイルは、ファイル単位で操作します。
- 「Ignore whitespace」がオンの間は、行単位の操作はできません。
- 実行する直前に差分を取り直し、表示していた差分と同じであることを確認します。表示後にファイルが変わっていた場合は、何も変更せずにエラーを表示します。
- 操作した直後は、差分を読み直すまでの間、行単位の操作を無効にします。
- 末尾に改行がない行の後ろに行を足すような選択は、行がつながってしまうため実行しません。改行の変更を含む隣の行も一緒に選んでください。

**コミットの作成者**
- git の設定（`user.name` / `user.email`）をそのまま使います。
- 未設定のリポジトリでは、git のエラーがそのまま表示されます。

**自動更新**
- 作業ツリーと `.git` の変更を監視し、ステータス・ブランチ・履歴を自動で再読込します。
- git が無視するファイル（ビルド出力など）の変更では再読込しません。
- inotify の監視数の上限などで監視できない場合は、F5 キーかウィンドウへのフォーカスで再読込します。

## 開発

```bash
cargo test                                               # 単体テスト・結合テスト
cargo clippy --all-targets -- -D warnings
cargo test -p pengitto --test screenshots -- --ignored       # 画面を target/screenshots/*.png に出力
PENGITTO_PERF_REPO=/path/to/big/repo cargo test --release -p pengitto --test perf -- --ignored --nocapture
cargo run --release -p pengitto-bench -- /path/to/repo       # 履歴読込とグラフ計算の計測
cargo run --release -p pengitto-bench -- worktrees /path/to/repo   # worktree 一覧・LFS の判定の計測
# AppImage と RPM を実際に作って確かめる（時間がかかる）
PENGITTO_TEST_APPIMAGETOOL=… PENGITTO_TEST_APPIMAGE_RUNTIME=… cargo test -p pengitto --test m6_packages -- --ignored --test-threads=1
packaging/icons/render.sh                                # アイコンの PNG を元の画像から作り直す
./target/debug/pengitto --ime-probe                          # 日本語入力（IME）の確認画面
```

### 構成

| クレート | 内容 |
|---|---|
| `crates/pengitto-core` | GUI に依存しない git 層（CLI 実行、log/refs/status/diff の解析、コミットグラフのレーン計算） |
| `crates/pengitto` | eframe/egui のアプリ本体 |
| `crates/pengitto-bench` | 計測用の CLI |
