#!/usr/bin/env bash
# PPM 포크 웹 빌드: 한글 글꼴(NanumGothic, OFL-1.1)과 일본어 글꼴(BIZ UDPGothic Regular, OFL-1.1)을 넣어 dist/web 을 만든다.
# 이 두 글꼴만 들어간다(업스트림 릴리스에 있던 다른 글꼴은 빠진다). 일본어 글꼴은 ?lang=ja 화면과 일본어 주석에 쓴다.
# 필요: Rust 1.95+ (기본 1.99.1), wasm32-unknown-unknown target, trunk 0.21+
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
export CRAFT_FONTS_DIR="$root/ppm/fonts-ko"
export RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-1.99.1}"
app=pdfcraft-web
cd "$root/apps/$app"
# PPM 빌드는 항상 호스트 잠금(host-lock): 호스트 없이는 문서를 열지 않고 로컬 파일·인쇄·외부 링크를 막는다.
# 컴파일 시점 값이라 실행 중에 풀 수 없다.
# 인자 없음: release(PR·배포용). --fast: 로컬 확인용(LTO 없음, wasm-opt 건너뜀, 크기 큼).
case "${1:-}" in
  "") trunk build --release --features host-lock ;;
  --fast) trunk build --cargo-profile web-fast --features host-lock ;;
  *) echo "usage: $0 [--fast]" >&2; exit 2 ;;
esac
# OFL-1.1 §2: the license travels with the embedded font.
mkdir -p "$root/dist/web/licenses"
cp "$root/ppm/fonts-ko/fonts/nanum-gothic/OFL.txt" "$root/dist/web/licenses/NanumGothic-OFL.txt"
cp "$root/ppm/fonts-ko/fonts/biz-udpgothic/OFL.txt" "$root/dist/web/licenses/BIZUDPGothic-OFL.txt"
# 관리자 화면이 읽는 명령 목록. 명령 표와 같은지는 테스트(the_checked_in_commands_json_matches_the_table)가 확인한다.
cp "$root/ppm/commands.json" "$root/dist/web/commands.json"
