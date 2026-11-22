[English](README.md) | 日本語

# Kinect WinUSB

Kinect v1 (Kinect for Xbox 360 / Kinect for Windows) を、**コア分離のメモリ整合性 (HVCI) を有効にしたまま**使うための互換ランタイム

アプリケーションの実行フォルダに `kinect10.dll` を配置するだけで、Kinect for Windows SDK v1.x (v1.0 ~ v1.8) で作られたアプリ (ネイティブ / .NET、64bit アプリ / 32bit アプリ) が動作する。

対象は **64bit 版の Windows 10 / 11**。メモリ整合性があるのはこの環境のみであり、それ以前のバージョンや 32bit 版では純正ドライバをそのまま利用できるため、本プロジェクトではサポートされない。


## 動作要件

- Windows 10 (x64) / Windows 11
- [Kinect for Windows Runtime v1.8](https://www.microsoft.com/en-us/download/details.aspx?id=40277) インストール済み
  - v1.5 ~ v1.7 のランタイムはサポートしているが実機では未確認
  - v1.0 のランタイムには対応しない
  - 最新の v1.8 は後方互換があるため、特に理由がなければ v1.8 の利用を推奨する
- Kinect v1
  - Kinect for Windows
  - Kinect for Xbox 360

## 使い方

1. Kinect のドライバを変更する
   - `kinect-winusb install` を実行 (管理者権限が必要)
1. アプリ・ゲームの実行ファイルと同じフォルダに、実行ファイルと同じアーキテクチャの `kinect10.dll` をコピーする

### 既にメモリ整合性を無効化して Kinect v1 を使用している場合

1. Windows セキュリティ ダッシュボードからメモリ整合性を有効化
1. 互換性のないドライバとして、Kinect のドライバが検出される
1. 対象ドライバの公開名 (`oemXX.inf`) を確認し、メモしておく
1. コマンドプロンプトを管理者として実行する
1. 以下のコマンドを実行しドライバを削除する
   - `pnputil /delete-driver oemXX.inf /uninstall /force` (`oemXX.inf` はメモした名前に置き換える)
1. メモリ整合性を再度有効化する
   - 他にも検出されたドライバがある場合、有効化できないため個別に対応が必要

## 開発者向け

### crates

| crate | 内容 |
|---|---|
| `crates/kinect10` | プロキシ Kinect10.dll (export のスタブ、純正 DLL のロード、IAT フック) |
| `crates/kinect-usb` | ユーザーモード版 kinectcamera.sys (WinUSB、プロトコル、フレーム組み立て、ロガー) |
| `crates/kinect-tool` | kinect-winusb.exe (install / uninstall / probe / camtest / nuitest、PnP 列挙、ハブ経由のディスクリプタ取得) |

### ビルド

```bash
cargo build --release
cargo build --release --target i686-pc-windows-msvc
```

### 診断コマンド

```bash
# Kinect の各 USB 機能、割り当てドライバ、カメラのディスクリプタを表示する。ハブ経由で読むので、ドライバ未割り当てでも動作する
kinect-winusb probe

# 純正 DLL を使わず、ユーザーモードドライバ単体で深度とカラーを受信する。`depth.pgm` と `color_bayer.pgm` を出力する
kinect-winusb camtest 90

# exe の隣の `kinect10.dll` (プロキシ) 経由で NUI API を動かす。`nui_color.ppm` と `nui_depth.pgm` を出力する。`--no-shutdown` を付けると `NuiShutdown` を呼ばずに終了し、後始末をしないアプリの終わり方を再現する
kinect-winusb nuitest --seconds 10
```

### .NET (`Microsoft.Kinect.dll`) 用のテスト

ビルド方法はそれぞれのファイル冒頭に記載。

| ファイル | 内容 |
|---|---|
| `tests/managed/KinectSmoke.cs` | カラー・深度・骨格が届くかだけを見る短いテスト |
| `tests/managed/KinectFeatures.cs` | カラー全形式・深度全解像度・骨格モード・座標変換・チルト・音声・停止と再開を順に確かめ、`PASS` / `FAIL` / `INFO` を出す。`hotplug <秒>` で抜き差しの確認も可能 |

ログは次の環境変数で制御できる。

| 変数 | 意味 |
|---|---|
| `KINECT_WINUSB_LOG=<path>` | ログをファイルに追記 |
| `KINECT_WINUSB_LOG_LEVEL=0..3` | 0=エラー、1=情報 (既定)、2=IOCTL、3=全 I/O |

レベル 1 以下のメッセージは `OutputDebugString` にも出力される。

### 仕組み

Kinect for Windows Runtime v1.8 のドライバのうち、Device (モーター)、Audio Array、Security は元々 WinUSB で動作する。カーネルドライバが必要なのは **カメラ用の `kinectcamera.sys` (KMDF) のみ**。このドライバがメモリ整合性に対応していないため、カメラだけが「Xbox NUI Camera (コード28)」のまま使用できない。

このプロジェクトは `kinectcamera.sys` の動作をユーザーモードで再実装し、純正ランタイムをそこにつなぎ替える。
骨格追跡、プレイヤーインデックス、座標変換などは純正 `Kinect10.dll` がそのまま処理する。差し替えているのはカメラドライバ相当の部分のみ。

```
アプリ (NUI API / Microsoft.Kinect.dll)
   │  LoadLibrary("Kinect10.dll")
   ▼
kinect10.dll (このプロジェクト・プロキシ)
   │  全エクスポートを同名・同序数の小さなスタブ (jmp 1命令) で純正へ渡す
   │  DllMain で純正 DLL を読み込み、その import だけを書き換える (IAT フック)
   ▼
純正 Kinect10.dll (System32 / SysWOW64) ……骨格追跡などはすべて純正のまま
   │  フック対象: CreateFileA/W, DeviceIoControl, ReadFile, CloseHandle,
   │             CreateIoCompletionPort, GetOverlappedResult
   ▼ カメラ ({00873FDF-61A8-11D1-EF5E-00C04F2D728B}) 宛ての I/O だけ横取り
kinect-usb (ユーザーモード版 kinectcamera.sys)
   │  カメラコマンドプロトコル、約40個の IOCTL、アイソクロナス転送 → フレーム組み立て
   ▼
WinUSB (Windows 標準・Microsoft 署名の winusb.sys) ── Kinect カメラ
```

### 純正ドライバとの違い

- **ストリーム停止**: 停止時に `WinUsb_AbortPipe` を使用しない。アイソクロナスパイプを abort すると、以後の転送がすべて `ERROR_BUSY` になるため。キュー済みの転送が自然に終わるのを待つので、停止に最大で約 100ms かかる
- **初回オープン**: プロセス内で最初にカメラを開いた際、念のためストリームを停止するコマンドを送る (前のプロセスが落ちて放置されたストリームを止めるため)
- **クローズ**: ハンドルを閉じると、そのハンドルで受信中の読み込みもキャンセルする。閉じたあとのバッファに書き込まないため
- **プロセス終了中**: アプリが `NuiShutdown` を呼ばずに終了すると、純正ランタイムは DLL のアンロード時 (`DLL_PROCESS_DETACH`) にストリーム停止とレーザー消灯を送る
- **Cycle port (0x220070)**: WinUSB では対応できないため、何もせず成功を返す

## License

[MIT LICENSE](LICENSE)
