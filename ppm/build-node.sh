#!/usr/bin/env bash
# PPM 포크 Node 모듈 빌드: 주석 목록·내보내기·제거·합치기(apps/ppm-annot-node)를
# wasm-bindgen --target nodejs 로 dist/pdf-node 에 만든다. 서버(worker)가 이 폴더를 읽는다.
# 한글 글꼴(NanumGothic, OFL-1.1)이 wasm 에 들어가야 한글 주석 외형이 그려진다(웹 빌드와 같은 이유).
# 필요: Rust 1.95+ (기본 1.99.1), wasm32-unknown-unknown target, wasm-bindgen CLI 0.2.129
#       (Cargo.lock 의 wasm-bindgen 과 같은 버전이어야 한다: `cargo install wasm-bindgen-cli --version 0.2.129`)
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
export CRAFT_FONTS_DIR="$root/ppm/fonts-ko"
export RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-1.99.1}"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"
out="${1:-$root/dist/pdf-node}"
bindgen="${WASM_BINDGEN:-wasm-bindgen}"
want=0.2.129
if ! command -v "$bindgen" >/dev/null || [ "$("$bindgen" --version | awk '{print $2}')" != "$want" ]; then
  echo "wasm-bindgen CLI $want 이 필요합니다 (WASM_BINDGEN=경로 로 지정할 수 있음)" >&2
  exit 1
fi
cd "$root"
cargo build -p ppm-annot-node --target wasm32-unknown-unknown --release
rm -rf "$out"
"$bindgen" --target nodejs --out-dir "$out" target/wasm32-unknown-unknown/release/ppm_annot_node.wasm
# 번들 글꼴의 라이선스는 산출물과 함께 간다(OFL-1.1 §2).
cp "$root/ppm/fonts-ko/fonts/nanum-gothic/OFL.txt" "$out/NanumGothic-OFL.txt"
cp "$root/ppm/fonts-ko/fonts/biz-udpgothic/OFL.txt" "$out/BIZUDPGothic-OFL.txt"
echo "built: $out"
