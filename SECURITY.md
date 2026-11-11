# セキュリティ

## 脆弱性の報告

GitHub のリポジトリで **Security → Advisories → Report a vulnerability** を使い、非公開で報告してください。ボタンが表示されない場合は、公開 Issue で非公開の連絡手段を問い合わせてください。

報告には `upscale --version` の結果、macOS のバージョン、再現手順、想定される影響を含めてください。添付する画像やログからは個人情報と認証情報を取り除いてください。 [GitHub の報告手順](https://docs.github.com/en/code-security/how-tos/report-and-fix-vulnerabilities/report-privately)

## 配布物と画像入力

配布物の `manifest.json` に推論エンジン・モデルの出典と SHA-256 を記録しています。`upscale doctor` でこれらのファイルの整合性と GPU 推論を確認できます。

同梱する Real-ESRGAN ncnn Vulkan v0.2.0 は、CVE-2023-4863 の修正前の libwebp を静的リンクしています。`bin/upscale` が受け付けるのは PNG と JPEG で、同梱エンジンには再エンコードした RGB PNG が渡ります。`libexec/realesrgan-ncnn-vulkan` を直接起動して外部から受け取った WebP を処理する用途は避けてください。 [採用したエンジンの実装](https://github.com/xinntao/Real-ESRGAN-ncnn-vulkan/blob/v0.2.0/src/webp_image.h) · [libwebp の修正](https://github.com/webmproject/libwebp/releases/tag/v1.3.2)
