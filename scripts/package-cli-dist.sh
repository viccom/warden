#!/usr/bin/env bash
# warden CLI 发行包组装脚本。
#
# 产出(默认 dist/ 下,每个平台一个包 + 校验和):
#   warden-<ver>-cli-x86_64-linux.tar.gz   .sha256
#   warden-<ver>-cli-x86_64-windows.zip    .sha256
#
# 每个包内含:二进制 + config/services.toml(平台化开箱配置)
#            + config/services.example.toml(全字段参考)+ AGENT-GUIDE.md
#            + README.md(包内快速上手)+ LICENSE
#
# 用法:
#   scripts/package-cli-dist.sh                          # 下载 release 资产组装双平台包
#   scripts/package-cli-dist.sh --bin-dir target/release # 用本地构建产物(不联网)
#   scripts/package-cli-dist.sh --platform linux-x86_64  # 只做 Linux 包(CI 分平台用)
#   scripts/package-cli-dist.sh --tag 0.3.0 --out /tmp/pkg
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLATFORMS_ALL=("linux-x86_64" "windows-x86_64")

TAG=""
OUT="$REPO_ROOT/dist"
BIN_DIR=""
PLATFORMS=()

usage() {
  # 打印文件头的注释块(第 2 行起,遇到首个非注释行即停)
  awk 'NR>1 { if (/^#/) { sub(/^# ?/, ""); print } else { exit } }' "${BASH_SOURCE[0]}"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --tag)      TAG="${2:?}"; shift 2 ;;
    --out)      OUT="${2:?}"; shift 2 ;;
    --bin-dir)  BIN_DIR="${2:?}"; shift 2 ;;
    --platform) PLATFORMS+=("${2:?}"); shift 2 ;;
    -h|--help)  usage; exit 0 ;;
    *) echo "未知参数:$1(见 --help)" >&2; exit 2 ;;
  esac
done

# 版本:未指定则取 Cargo.toml 的 version(包名用去 v 前缀的版本号)
[ -n "$TAG" ] || TAG="$(grep -m1 '^version' "$REPO_ROOT/Cargo.toml" | cut -d'"' -f2)"
PKGVER="${TAG#v}"
[ ${#PLATFORMS[@]} -gt 0 ] || PLATFORMS=("${PLATFORMS_ALL[@]}")

RELEASE_BASE="https://github.com/viccom/warden/releases/download/$TAG"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

log() { printf '[package] %s\n' "$*"; }
die() { printf '[package] 错误:%s\n' "$*" >&2; exit 1; }

# 取二进制:优先 --bin-dir 本地构建产物,否则下载 release 资产(名义名:warden-<tag>-x86_64-<plat>[.exe])
fetch_bin() {
  local platform="$1" dest="$2"
  local asset="warden-$TAG-x86_64-${platform%-x86_64}"
  local plain="warden"
  case "$platform" in
    windows-*) asset="$asset.exe"; plain="warden.exe" ;;
  esac
  if [ -n "$BIN_DIR" ]; then
    # 本地构建产物名为 warden / warden.exe(不带版本号),release 资产名带版本号
    local cand
    for cand in "$BIN_DIR/$asset" "$BIN_DIR/$plain"; do
      [ -f "$cand" ] && { cp -f "$cand" "$dest"; break; }
    done
    [ -s "$dest" ] || die "本地产物不存在:$BIN_DIR/$asset 或 $BIN_DIR/$plain(先 cargo build --release --jobs 6)"
  else
    log "下载 $RELEASE_BASE/$asset"
    curl -fsSL --retry 3 -o "$dest" "$RELEASE_BASE/$asset" \
      || die "下载失败:$RELEASE_BASE/$asset(GitHub 匿名限流时可稍后重试,或用 --bin-dir 本地构建)"
  fi
  [ "$(wc -c <"$dest")" -gt 1000000 ] || die "二进制过小($(wc -c <"$dest") 字节),疑似下载到错误内容"
}

# 宿主平台能跑就打一次 --version(开箱可用性实测;跨平台时跳过并声明)
smoke_check() {
  local platform="$1" exe="$2"
  local host; host="$(uname -s)"
  local runnable=0
  case "$platform" in
    linux-x86_64)   [ "$host" = "Linux" ] && runnable=1 ;;
    windows-x86_64) case "$host" in MINGW*|MSYS*|CYGWIN*) runnable=1 ;; esac ;;
  esac
  if [ "$runnable" = 0 ]; then
    log "跳过 $platform 冒烟(宿主 $host 无法执行该平台二进制)"
    return 0
  fi
  local got; got="$("$exe" --version 2>&1 | head -1)"
  case "$got" in
    "warden $PKGVER") log "冒烟通过:$exe --version → $got" ;;
    *) die "冒烟失败:$exe --version → '$got'(期望 'warden $PKGVER')" ;;
  esac
}

# 打包:linux → tar.gz(保留可执行位);windows → zip(回退 7z / Compress-Archive)
make_archive() {
  local platform="$1" pkgdir="$2" archive="$3"
  case "$platform" in
    linux-*)
      tar -czf "$archive" -C "$(dirname "$pkgdir")" "$(basename "$pkgdir")"
      ;;
    windows-*)
      if command -v zip >/dev/null 2>&1; then
        (cd "$(dirname "$pkgdir")" && zip -qr "$archive" "$(basename "$pkgdir")")
      elif command -v 7z >/dev/null 2>&1; then
        7z a -tzip "$archive" "$pkgdir" >/dev/null
      elif command -v powershell >/dev/null 2>&1; then
        # CI 的 windows runner 走这条:Compress-Archive -Path <目录> 会保留包目录名
        powershell -NoProfile -Command \
          "Compress-Archive -Path '$(cygpath -w "$pkgdir" 2>/dev/null || echo "$pkgdir")' -DestinationPath '$(cygpath -w "$archive" 2>/dev/null || echo "$archive")' -Force"
      else
        die "缺少打包工具:zip / 7z / powershell 均不可用"
      fi
      ;;
  esac
}

mkdir -p "$OUT"
declare -a ARCHIVES=()

for platform in "${PLATFORMS[@]}"; do
  case "$platform" in
    linux-x86_64)   binname="warden";     demo="services.demo.linux.toml" ;;
    windows-x86_64) binname="warden.exe"; demo="services.demo.windows.toml" ;;
    *) die "不支持的平台:$platform(仅 linux-x86_64 / windows-x86_64)" ;;
  esac

  pkgname="warden-$PKGVER-cli-$platform"
  # 暂存目录放临时区,不落在 --out 里(产物目录只留归档与校验和)
  pkgdir="$WORK/stage/$pkgname"
  rm -rf "$pkgdir"; mkdir -p "$pkgdir/config"

  fetch_bin "$platform" "$pkgdir/$binname"
  chmod +x "$pkgdir/$binname" 2>/dev/null || true

  # 开箱配置:平台演示配置 → services.toml(被 exe 同级 config/ 自动发现)
  cp "$REPO_ROOT/config/$demo" "$pkgdir/config/services.toml"
  cp "$REPO_ROOT/config/services.example.toml" "$pkgdir/config/services.example.toml"
  cp "$REPO_ROOT/docs/AGENT-GUIDE.md" "$pkgdir/AGENT-GUIDE.md"
  cp "$REPO_ROOT/packaging/cli/README.md" "$pkgdir/README.md"
  cp "$REPO_ROOT/LICENSE" "$pkgdir/LICENSE"

  smoke_check "$platform" "$pkgdir/$binname"

  case "$platform" in
    linux-*)   archive="$OUT/$pkgname.tar.gz" ;;
    windows-*) archive="$OUT/$pkgname.zip" ;;
  esac
  make_archive "$platform" "$pkgdir" "$archive"
  (cd "$(dirname "$archive")" && sha256sum "$(basename "$archive")" >"$(basename "$archive").sha256")
  ARCHIVES+=("$archive")
  log "已产出 $archive ($(wc -c <"$archive") 字节)"
done

log "完成。包内布局:"
for a in "${ARCHIVES[@]}"; do
  log "  $(basename "$a")"
  case "$a" in
    *.tar.gz) tar -tzf "$a" | sed 's/^/    /' ;;
    *.zip)    (command -v unzip >/dev/null 2>&1 && unzip -Z1 "$a" || echo "    (unzip 不可用,跳过列表)") | sed 's/^/    /' ;;
  esac
done
