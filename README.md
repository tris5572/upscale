# upscale

Apple Silicon Mac 向けの画像拡大 CLI。PNG・JPEG を Real-ESRGAN で処理し、各辺を 2〜4 倍にした PNG を保存する。

## インストール

Apple Silicon（arm64）向け。実機検証は Apple M4・macOS 27.0.1 で実施した。機能・画質・性能の測定結果は [実機検証](VALIDATION.md) に記載している。

GitHub の Releases から配布アーカイブと SHA-256 ファイルを取得するか、下記の手順でビルドする。アーカイブを展開し、`upscale-macos-arm64/bin/upscale` を起動する。推論エンジン、モデル、診断データは同梱される。実行時に Python やネットワーク接続は不要。

```sh
shasum -a 256 -c upscale-0.1.0-macos-arm64.tar.gz.sha256
tar -xzf upscale-0.1.0-macos-arm64.tar.gz
./upscale-macos-arm64/bin/upscale doctor
```

配布バイナリはアドホック署名で、Apple の公証は受けていない。Gatekeeper の警告が出た場合は SHA-256 を照合し、**システム設定 → プライバシーとセキュリティ**で該当する実行ファイルの起動を許可する。 [Apple の手順](https://support.apple.com/ja-jp/102445)

## 使い方

```sh
./upscale-macos-arm64/bin/upscale input.png
./upscale-macos-arm64/bin/upscale input.jpg -o output.png --scale 2
./upscale-macos-arm64/bin/upscale input.png -o output.png --preset anime --tile 128
./upscale-macos-arm64/bin/upscale ./inputs -o ./outputs --scale 3
./upscale-macos-arm64/bin/upscale models list
```

- 既定値は `--scale 4 --preset general --tile auto`。2 倍・3 倍も 4 倍で推論し、Lanczos3 で指定寸法に縮小する。
- `general` は写真や一般画像、`anime` はアニメやイラスト向け。どちらも既定の GPU を使用する。
- `--tile auto` で Vulkan のメモリ不足を検出した場合、入力寸法に応じて 32px のタイルで最大 1 回再試行する。固定タイルでは指定値を維持する。
- `-o` を省略すると、入力と同じディレクトリに PNG を保存する。たとえば `sample.png` の 4 倍出力は `sample_x4.png` になり、同名の項目があれば `sample_x4(1).png`、`sample_x4(2).png` と空いている名前を探す。
- ディレクトリ指定時は実行開始時に直下にある対象ファイルをファイル名順に処理する。同じディレクトリに保存した前回の出力も、再実行時の入力に含まれる。`-o` を指定した場合は PNG を出力し、既存出力との衝突を処理開始前に検査する。
- 出力は 8-bit RGB または RGBA。EXIF の向きと入力の色を画素に反映し、PNG のチャンクを IHDR・IDAT・IEND に限定する。
- 入力は静止画のみ。16-bit PNG、HDR、CMYK JPEG、APNG はエラーになる。対応する色指定は RGB/グレーの埋め込み ICC、sRGB、PNG の gAMA・cHRM、およびフルレンジの sRGB/Display P3 cICP。色指定がなければ sRGB として扱う。
- `--quiet` は進捗と成功表示を抑制する。終了コードは成功 `0`、処理失敗 `1`、引数・形式・出力指定の不正 `2`、配布物または GPU 環境の不良 `3`、割り込み `130`。
- 中断された処理の作業ファイルは、次に同じ出力先ディレクトリを使用するときに回収する。実行中の処理はファイルロックで保護する。

## ビルドと配布

配布用アーカイブは手元の Apple Silicon Mac で作成する。GitHub Actions は `cargo fmt`、Clippy、テストを実行する。

macOS arm64 の Rust 1.89 以降、Xcode Command Line Tools、`jq` が必要。配布スクリプトは Rust ツールチェーンの標準ライブラリの著作権・ライセンス情報も同梱する。rustup で取得する場合は `rust-docs` コンポーネントに含まれる。

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
./scripts/package.sh
```

配布スクリプトは固定した公式アーカイブの SHA-256 を照合し、`dist/upscale-0.1.0-macos-arm64.tar.gz` と同名の `.sha256` ファイルを作成する。取得済みの公式 ZIP は `./scripts/package.sh /path/to/upstream.zip` で指定できる。

`cargo run` や `target/release/upscale` で画像処理を起動すると、モデルなどの配布リソースが不足する。画像処理と `doctor` には、アーカイブを展開したディレクトリの `bin/upscale` を使用する。

GitHub Releases でバイナリを配布する際は、展開後の `doctor` を Apple Silicon 実機で実行し、アーカイブと `.sha256` をリリースに添付する。ビルドキャッシュ、配布物、ユーザー画像は Git の管理対象から除外する。

## ライセンスとセキュリティ

upscale のコードは [MIT ライセンス](LICENSE)。Real-ESRGAN、学習済みモデル、同梱ライブラリには各上流のライセンスが適用される。採用版・出典・SHA-256 は配布物の `manifest.json`、上流コンポーネントの出典と通知は [licenses](licenses/Upstream-components.txt) に記載している。

脆弱性の報告方法と同梱エンジンの注意事項は [セキュリティポリシー](SECURITY.md) を参照。
