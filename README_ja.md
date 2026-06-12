# godot-gsplat

[English](README.md) | 日本語

`godot-gsplat` は、PC・モバイル・VR で 3D Gaussian Splatting を表示するための Godot 4 アドオンです。

スプラットは `KHR_gaussian_splatting` glTF 拡張（godot-rust で実装）を通じてインポートされ、テクスチャ駆動の低レベル `RenderingServer` パスでレンダリングされます。異方性スプラットはシェーダ内で射影され（アフィン／ヤコビアン近似を用いない厳密な楕円体射影）、GPU 上でデプスソートされ、フラグメントごとに評価される球面調和関数（SH）でシェーディングされます。ランタイムパスは `Node3D` を基本とするため、Quest ネイティブやその他のコンポジタ非依存の構成をターゲットにできます。

## 目標 (Goals)

- godot-rust で実装した `GLTFDocumentExtension` を通じて `KHR_gaussian_splatting` をインポートする。
- ランタイムパスを `Node3D` 基本に保ち、Quest ネイティブやその他のコンポジタ非依存ターゲットで動作させる。
- PC・モバイル・VR で単一のデータモデルを共有する。
- オプションの `KHR_gaussian_splatting_compression_spz_2` 対応の余地を残す。

## 設計方針 (Design Principles)

- インポートファーストのアーキテクチャ。
- ステートレスな `GLTFDocumentExtension` 実装。
- インポートデータ・ランタイムノードの状態・レンダリングバックエンドの状態を明確に分離する。
- PC・モバイル・VR で共有するデータモデル。

## 仕組み (How it works)

### インポート

- `GltfGsplatDocumentExtension`（`GLTFDocumentExtension`）が `KHR_gaussian_splatting` 拡張を解析し、スプラットごとの属性（位置・回転・スケール・不透明度・SH 係数）をインターリーブされた float32 ペイロードにデコードして、`GaussianSplatNode3D` に紐付いた `GaussianSplatAsset` リソースを生成します。
- ペイロードはインポート時に空間グリッドへ分割され、ランタイムが大規模クラウドの一部分のみを限定的にストリーミングできるようにします。
- 大きなキャプチャは `.gsplatpack` に変換することもできます。ランタイムは `user://` 上のページ単位パックを読み込み、デコード済みペイロード全体を一度にメモリへ載せないため、Quest のようなメモリ制約の強い環境に向いた経路になります。
- `GsplatScenePostImportPlugin` が glTF インポートダイアログにプレビューオプション（`gsplat/preview_max_splats`、`gsplat/preview_max_splat_radius`、`gsplat/preview_scale_multiplier`）を公開します。

### エディタのドラッグ＆ドロップ

- `.gltf`/`.glb` を 3D ビューポートにドロップすると、インポートされたシーンのラッパーが、`Source glTF` プロパティにドロップしたファイルを指す素の `GaussianSplatNode3D` に置き換えられます。ノードはその後スプラットをライブロードするため、シーンはコピーを焼き込む代わりにソースアセットを参照します。

### レンダリング

- 各スプラットは低レベル `RenderingServer` メッシュサーフェスから出す仮想クアッドです。サーフェスは空の頂点配列と `splat_count * 6` 個の仮想頂点を使い、シェーダが `VERTEX_ID` からスプラットスロットとクアッド角を導出します。そのため、スプラットごとの transform ストリームはありません。スプラットごとのデータ（中心・軸ごとのスケール・回転クォータニオン・色・SH 係数）は、シェーダがサンプリングするテクスチャにパックされます。
- 頂点シェーダは、**アフィン／ヤコビアン近似を用いずに**スプラット楕円体をスクリーン空間の楕円へ射影します。すなわち、中心と回転・スケール済みの 3 本の半軸をクリップ空間に射影し、射影された二次曲面（コニック）を構成して、固有値分解で楕円を厳密に復元します。これにより、VR で重要となる「カメラが近く広 FOV」の視点でも安定します（従来のヤコビアン近似ではボケや形状ドリフトが生じる条件）。その後、楕円の軸方向にクアッドを拡張し、画面外のスプラットをフラスタムカリングします。
- サブピクセルのフットプリントにはエネルギー保存型アンチエイリアスを適用します。スクリーン上の楕円を最小サイズにクランプし、アルファを面積比でスケールするため、遠方のスプラットが過剰に明るくならずに視認できます。
- コーナーごとの深度（既定の `splat_depth_mode = ray`）は、視線レイ上のガウシアンのピーク（レイと楕円体の最近接）として求め、`POSITION.z` に書き込みます。これにより early-z を維持したまま、不透明ジオメトリとの交差（遮蔽）が正しく行われます。`center` モードは各コーナーにスプラット中心の深度を書き込み、コーナーごとの精度と引き換えに、ray 経路が高αの重なりスプラットで生じさせる VR 白チラつきを除去します（後述の「VR 実行時の設定」を参照）。
- フラグメントシェーダはガウシアン減衰を適用し、サブピクセルの寄与を破棄します。
- 視点依存の色は、シェーダ内で評価される球面調和関数（次数 0〜3）から得られます。
- 厳密射影／AA／レイ深度の数式は [VRChatGaussianSplatting](https://github.com/MichaelMoroz/VRChatGaussianSplatting)（MIT）を参考にしています。

### デプスソート

- GPU コンピュートによるカウンティングソート（16 ビットのデプスバケットと多段プレフィックスサム）が、視点が変化するたびにスプラットを奥から手前へ並べ替えます。静止視点では再ソートをスキップします。VR 向けに視点（目）ごとのソートパスがあります。
- **制限事項 — 複数カメラの同時描画。** ノードは奥から手前への並び順を 1 つだけ保持し、それは単一のカメラ視点で計算されます。同一フレーム内で同じノードが複数のアクティブカメラから描画される場合（分割画面、ピクチャインピクチャ、ミラー、1 つの `World3D` を共有する複数の SubViewport など）、正しくブレンドされるのはその 1 台のカメラ視点のみで、他の視点では前後関係が誤って合成されます。スプラットの幾何（位置・形状）はどのビューポートでも正しく、共有されているのはアルファ合成順だけです（1 つのビューポート内に両眼を持つ VR マルチビューは別経路で処理され、この制限の影響を受けません）。同じクラウドを複数カメラから同時に表示したい場合は、ノードをビューポートごとに複製し、それぞれ独立した `World3D` に置いてください。複数カメラへの一般的な対応は予定していません。

### アンチエイリアス（MSAA）

- Gaussian スプラットは MSAA の恩恵を受けません。スプラットはジオメトリのエッジではなくアルファブレンドされるビルボードであり、MSAA はコストを増やすだけです。とくに塗りつぶし帯域が乏しい VR/Quest で顕著です。サブピクセルのフットプリントはシェーダ自身のエネルギー保存型アンチエイリアスで処理されます。
- Godot の `rendering/anti_aliasing/quality/msaa_3d` は既定で *Disabled* であり、これがスプラットシーンの推奨設定です。他のジオメトリのために MSAA を有効化する場合、スプラットには視覚的な利点がほとんどないままコストが増える点に注意してください。とくに XR では `msaa_3d` を無効のままにすることを推奨します。

### VR 実行時の設定

- VR では `render_profile = XR` を使います。これは VR-safe パイプライン、ヘッドセンター基準のソート、ヘッドセット描画向けの低めの SH・上限の設定、そして `splat_depth_mode = center` を選びます。
- `splat_depth_mode = center` は、per-corner の ray 深度経路がステレオで高αの重なりスプラットに生じさせる定常的な白チラつきを除去します。`XR` プロファイルは既定でこれを有効にし、他のプロファイルは `ray` のままでバックエンド設定（または項目別の上書き）からオプトインできます。
- VR では `rendering/anti_aliasing/quality/msaa_3d` を *Disabled* のままにします。スプラット描画は MSAA に依存せず、Quest や PC-VR では無効化したほうが余計なコストを避けられます。

### レンダープロファイル

`render_profile` は `GaussianSplatNode3D` で品質を決める主要な設定項目です。各プリセットは、バックエンドのパイプラインターゲット・スプラット上限・SH 次数・VR ビュー基準・スプラット深度モードに解決されます:

| プロファイル | パイプライン（ターゲット） | スプラット上限 | SH 次数 | 深度モード | こんなときに |
|---|---|---|---|---|---|
| `Low` | VR-safe | 約150k | 0 | `ray` | 最小コスト。非力な GPU や、強く上限を掛けたい大規模クラウド向け。 |
| `Middle` | Mobile | 約500k | 1 | `ray` | モバイル／デスクトップのバランス型。 |
| `High` | Desktop | 無制限（全スプラット） | 3 | `ray` | 高性能デスクトップ GPU での最高品質。フル SH・上限なし。 |
| `XR` | VR-safe | 300k〜800k（アセットの空間範囲に応じてスケール） | 1 | `center` | VR／ヘッドセット描画。`center` 深度で、`ray` 経路が高αの重なりスプラットに生じさせるステレオ白チラつきを回避。 |
| `Custom` | 手動 | 手動 | 手動 | 手動 | 完全な手動制御。個別フィールドを編集すると自動的にこれになります。 |

- すべてのプリセットは **head-center** の VR ビュー基準を使用します。per-eye はバックエンド設定からのみ切り替えられます（per-eye ソートは実機未検証）。
- スプラット上限は常にアセットの点数にクランプされるため、小さなクラウドは `High` でも全点が描画されます。
- `XR` の上限は、空間範囲 約2単位で 300k、約30単位で 800k の間を補間します（テーブルトップ撮影は少なめ、建物スケールは多め。`xr_adaptive_budget`）。
- 個別フィールド（上限・SH 次数・深度モードなど）を手動で編集すると、プロファイルは `Custom` に切り替わり、アセットの再バインド時にプリセットが再適用されなくなります。
- プロファイルは設定一式に解決され、スクリプトから `get_profile_settings` / `apply_profile_settings` で読み取り・項目別の上書き・再適用ができます（例: `High` のまま深度モードだけを上書き）。`demo/minimal_demo.gd` を参照。

### 調整手順

| 手順 | 何を設定するか | 決め方 |
|---|---|---|
| 1 | `render_profile` | まず基準を決めます。Quest / XR では `XR` を基本にし、高品質ソースを優先してあとから手動調整するなら `High`、複数項目をすでに上書きする前提なら `Custom` を使います。 |
| 2 | `get_profile_settings(profile)` | プロファイルの辞書を取り出し、残したい項目はそのままにします。プリセットから外したい値だけを上書きします。 |
| 3 | `budget` / `sh_degree` / `splat_depth_mode` | まず描画コストを詰めます。Quest では `budget` を先に下げるのが最も効きやすく、その次に `sh_degree` を落とします。ステレオの白チラつきが出るなら `splat_depth_mode = center` を維持します。 |
| 4 | `splat_chunk_selection = view_priority` | レンダー予算が収まったらストリーミングを有効化します。現在の視界を優先し、その後で遠方・周辺チャンクを段階的に落とします。 |
| 5 | `view_priority_target_budget` / `view_priority_min_lod_per_chunk` / `view_priority_fov_degrees` / `view_priority_full_distance` | ストリーミング選択の調整はこの順で進めます。まずターゲット予算、次に各チャンクに残す最小 LOD、最後に FOV と full-distance を触ります。最初の 2 つが全体の仕事量を決め、後ろの 2 つがどの部分をフル密度に残すかを決めます。 |

`get_profile_settings` が返し `apply_profile_settings` が受け取る辞書には次の 5 つのキーがあります。一部のキーだけを含む辞書を渡せば、そのキーだけを上書きできます（含まれないキーはノードの現在値を維持します）:

| キー | 型 | 値 | 備考 |
|---|---|---|---|
| `target_hint` | String | `desktop` / `mobile` / `vr_safe` | バックエンドのパイプラインターゲット。即時適用され、アセットの再バインドをまたいで保持されます。 |
| `budget` | int | スプラット数、または `-1` | アクティブスプラットの上限（アセットの点数にクランプ）。`-1` はアセットの空間範囲から上限を導出します（`XR` の適応カーブ）。 |
| `sh_degree` | int | `0`〜`3` | シェーダで評価する球面調和関数の次数。 |
| `vr_view_basis` | String | `head_center` / `per_eye` | VR のソート／カリング基準。即時適用され、再バインドをまたいで保持されます。 |
| `splat_depth_mode` | String | `ray` / `center` | コーナーごとのレイ×楕円体深度、または平坦なスプラット中心深度。即時適用され、再バインドをまたいで保持されます。 |

- `target_hint`・`vr_view_basis`・`splat_depth_mode` はバックエンドターゲット系の項目で、即時に適用され、アセットの再バインドをまたいで保持されます。
- `budget` と `sh_degree` はバインド中のアセットに依存するため（適応上限はその空間範囲を必要とします）、アセットがバインドされた時点で適用され、再バインドのたびに再適用されます。これにより項目別の上書きが再バインド後も維持されます。

### 品質とスケーリングの制御
| 設定 | 型 | 意味 |
|---|---|---|
| `splat_chunk_selection` | String | `view_priority` は、カメラ／HMD の周囲に広い視野コーンを確保し、ターゲット上限を超えるときは遠方・周辺チャンクの密度を下げる方式です。Quest 向けの大きな室内シーンで使う主なストリーミングモードです。 |
| `view_priority_fov_degrees` | float | そのコーン幅です。既定の `200` 度は通常の視野角より広めにしてあり、チャンクの追加読み込み中でも急な首振りで空白が見えにくくしています。 |
| `view_priority_full_distance` | float | そのローカル空間距離内のチャンクを候補に残す半径です。小さくするとフル密度で保証される範囲が狭くなり、大きくすると近距離の詳細は増えますがアクティブなスプラット数も増えます。 |
| `view_priority_target_budget` | int | ストリーミング選択で使うスプラット上限です。視野コーン全体がこの上限を超える場合、領域ごと落とすのではなく、遠方・周辺チャンクから密度を下げます。 |
| `view_priority_min_lod_per_chunk` | int | 密度を下げるときに各チャンクへ残す最小プレフィックスです。小さいほどチャンクの網羅性が増え、大きいほど残ったチャンク内部の詳細が保たれます。 |
| `GaussianSplatBackendSettings` | `Resource` | ターゲットヒント（Desktop / Mobile / VR-safe）からレンダーパイプラインを解決し、VR ビュー基準（head-center / per-eye）とスプラット深度モード（ray / center）を保持します。 |

## コンポーネント (Components)

| クラス | 基底 | 役割 |
|---|---|---|
| `GaussianSplatNode3D` | `Node3D` | ランタイムノード。アセットをバインドするか `source_gltf` をライブロードし、レンダーデータを構築して GPU ソートを駆動する。 |
| `GaussianSplatAsset` | `Resource` | デコード済みのスプラットペイロード、レイアウト、AABB、任意のチャンクテーブル。 |
| `GaussianSplatCloudSettings` | `Resource` | クラウドごとの可視性、スケール、スプラット上限、SH 次数。 |
| `GaussianSplatBackendSettings` | `Resource` | ターゲット / パイプライン / VR ビュー基準 / スプラット深度モードの選択。 |
| `GltfGsplatDocumentExtension` | `GLTFDocumentExtension` | `KHR_gaussian_splatting` をインポートする。 |
| `GsplatScenePostImportPlugin` | `EditorScenePostImportPlugin` | インポートダイアログのプレビューオプション。 |

## ステータス (Status)

実装済み:

- godot-rust の GDExtension クラス登録とエディタプラグイン。
- `KHR_gaussian_splatting` glTF の `GaussianSplatAsset` へのインポート（可変ストライドペイロードでの高次 SH を含む）。
- 厳密な楕円体射影（アフィン／ヤコビアン近似なし）、エネルギー保存型アンチエイリアス、選択式のコーナー深度（レイ／中心）、頂点シェーダでのフラスタムカリング、スプラットごとの transform ストリーム削除を備えた、テクスチャ駆動の低レベル `RenderingServer` スプラットレンダラ。
- 適応的な再ソートゲーティングを備えた GPU コンピュートのデプスソート。
- レンダープロファイルでゲートされる、シェーダ内の球面調和関数評価（次数 0〜3）。
- Low / Middle / High / XR / Custom のレンダリング品質プロファイル。
- インポート時の空間グリッドチャンク分割と、限定的・重要度順・非同期再構築されるアクティブチャンク集合。
- ドロップした glTF を焼き込まずにソースとしてリンクするエディタのドラッグ＆ドロップ。

実装済みだが未検証:

- VR の視点（目）ごとのソートとレンダリング経路。実装はありますが、実機での検証は未実施です。

未実装:

- `KHR_gaussian_splatting_compression_spz_2` のデコード（拡張名を予約しているのみ）。

## 必要要件とビルド (Requirements & build)

- Godot 4.5+（プロジェクトは 4.7 で作成）。
- Rust ツールチェイン。GDExtension は `godot` クレート 0.5（`api-4-5`）を使用します。

拡張をビルドし、プロジェクトを開いて **Godot Gsplat** アドオンを有効化します:

```powershell
cargo build            # debug   -> target/debug/godot_gsplat.dll
cargo build --release  # release -> target/release/godot_gsplat.dll
```

`godot_gsplat.gdextension` は `res://target/{debug,release}/godot_gsplat.dll` を指します。アドオン（`addons/godot_gsplat`）は、glTF ドキュメント拡張、ポストインポートプラグイン、ビューポートのドロップフックを登録します。

## デモ (Demo)

`demo/minimal_demo.tscn`（プロジェクトのメインシーン）が、軌道カメラとともにサンプルクラウドを読み込みます。

### 巨大なクラウドのランタイムロード

数百万スプラットの glTF はパース＋デコードに数秒かかるため、デモでは
`GLTFDocument.append_from_file` / `generate_scene` をバックグラウンドの `Thread` で実行し、
生成された（ツリー未接続の）シーンの `add_child` だけをメインスレッドで行います。パターンは
`demo/minimal_demo.gd` を参照してください。ノード側でも、アクティブセットが約 50 万スプラットを
超えるレンダーセットの（再）構築は非同期チャンクリビルドワーカーに委譲され、メインスレッドを
ブロックしません（エディタではインポートが `.scn` にベイクを書き込むため常に同期構築です）。
配布物ではエディタでインポート済みのシーンを使うのが推奨です。デコードはインポート時の一度きりで、
ベイク済みレンダーは即座にロードされます。

## .gsplatpack への変換 (Converting glTF to .gsplatpack)

`.gsplatpack` は、Meta Quest のようにメモリ制約が厳しい環境で大きなスプラットクラウドを扱うためのランタイム形式です。スプラット全体をデコード済みペイロードとしてメモリに保持するのではなく、ディスク上のストリーミング可能なページとして扱います。

パックコンバーターの入力は、既存の `KHR_gaussian_splatting` `.gltf` または `.glb` です。元データが `.ply` の場合は、次のセクションのスクリプトで先に glTF へ変換してから、生成された glTF を `.gsplatpack` に変換してください。

Godot エディタでは次の手順で変換できます。

1. Godot Gsplat アドオンを有効にします。
2. `Project > Tools > Godot Gsplat Pack Converter` を開きます。
3. `Source glTF` に `.gltf` または `.glb` を指定します。
4. `Output pack` に `.gsplatpack` の出力先を指定します。`Use Default Output` を有効にすると、入力ファイルと同じ場所に同じベース名で出力されます。
5. `Convert` を押します。

生成したパックは、`GaussianSplatNode3D` の `source_gltf` やデモの `sample_path` など、アドオンがスプラット入力を受け付ける場所で使えます。Android や Quest へ export する場合は、`.gsplatpack` がエクスポートに含まれるよう、例えば `samples/converted/scene.gsplatpack` を export include filters に追加してください。

変換設定を変えて再生成する可能性がある場合は、元の glTF を正本として残しておいてください。

## スプラットの glTF への変換 (Converting splats to glTF)

`tools/ply_to_khr_gaussian_gltf.py` は、バイナリ・リトルエンディアンの 3DGS `.ply` を `KHR_gaussian_splatting` glTF（位置・回転・スケール・不透明度・SH 次数 0〜3・`COLOR_0` フォールバック）に変換します。使い方と座標系オプションは `tools/README.md` を参照してください。

## 参考プロジェクト (References)

本アドオンの実装にあたり参考にしたプロジェクト（実装のコピーではなく、アイデアと構造の参考）:

- [KhronosGroup/glTF](https://github.com/KhronosGroup/glTF) — `KHR_gaussian_splatting` 拡張の仕様。
- [MichaelMoroz/VRChatGaussianSplatting](https://github.com/MichaelMoroz/VRChatGaussianSplatting)（MIT） — 厳密な楕円体射影、エネルギー保存型アンチエイリアス、レイベースのコーナー深度。
- [ReconWorldLab/godot-gaussian-splatting](https://github.com/ReconWorldLab/godot-gaussian-splatting) — Godot 側のノード／リソース構造と VR のビュー／射影の扱い。
- [playcanvas/supersplat](https://github.com/playcanvas/supersplat) — データ処理とレンダーオーケストレーションの分離、スケーリングのアイデア。
- [BladeTransformerLLC/gauzilla](https://github.com/BladeTransformerLLC/gauzilla) — ローダー／デコーダー／レンダラーの分離と非同期・ストリーミングのアイデア。
- [godotengine/godot](https://github.com/godotengine/godot) — エディタのインポート挙動と glTF インポートオプションの慣習。

## ライセンス (License)

[MIT License](LICENSE) のもとで公開されています。
