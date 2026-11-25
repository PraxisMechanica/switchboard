#!/usr/bin/env sh
set -eu

REPO="${SWITCHBOARD_REPO:-PraxisMechanica/switchboard}"
VERSION="${SWITCHBOARD_VERSION:-v0.1.0}"
BASE_URL="${SWITCHBOARD_BASE_URL:-https://github.com/${REPO}/releases/download/${VERSION}}"
INSTALL_DIR="${SWITCHBOARD_INSTALL_DIR:-$HOME/.local/bin}"

OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}:${ARCH}" in
  Darwin:arm64|Darwin:aarch64) TARGET="aarch64-apple-darwin" ;;
  Darwin:x86_64)               TARGET="x86_64-apple-darwin" ;;
  Linux:x86_64)                TARGET="x86_64-unknown-linux-gnu" ;;
  Linux:aarch64|Linux:arm64)   TARGET="aarch64-unknown-linux-gnu" ;;
  *)
    echo "Unsupported platform: ${OS} ${ARCH}" >&2
    exit 1
    ;;
esac

ARCHIVE="switchboard-${VERSION}-${TARGET}.tar.gz"
TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

echo "Downloading ${ARCHIVE}..."
curl -fsSL "${BASE_URL}/${ARCHIVE}" -o "${TMPDIR}/${ARCHIVE}"
curl -fsSL "${BASE_URL}/SHA256SUMS.txt" -o "${TMPDIR}/SHA256SUMS.txt"

EXPECTED="$(grep " ${ARCHIVE}\$" "${TMPDIR}/SHA256SUMS.txt" | awk '{print $1}')"
if [ -z "${EXPECTED}" ]; then
  echo "Checksum entry missing for ${ARCHIVE}" >&2
  exit 1
fi

if command -v shasum >/dev/null 2>&1; then
  ACTUAL="$(shasum -a 256 "${TMPDIR}/${ARCHIVE}" | awk '{print $1}')"
elif command -v sha256sum >/dev/null 2>&1; then
  ACTUAL="$(sha256sum "${TMPDIR}/${ARCHIVE}" | awk '{print $1}')"
else
  echo "Missing shasum/sha256sum to verify download" >&2
  exit 1
fi

if [ "${EXPECTED}" != "${ACTUAL}" ]; then
  echo "SHA256 mismatch for ${ARCHIVE}" >&2
  exit 1
fi

mkdir -p "${INSTALL_DIR}"
tar -xzf "${TMPDIR}/${ARCHIVE}" -C "${TMPDIR}"
install -m 755 "${TMPDIR}/switchboard" "${INSTALL_DIR}/switchboard"

if [ "${OS}" = "Darwin" ] && command -v xattr >/dev/null 2>&1; then
  xattr -d com.apple.quarantine "${INSTALL_DIR}/switchboard" 2>/dev/null || true
fi

if [ -f "${TMPDIR}/SKILL.md" ]; then
  for SKILL_DIR in \
    "$HOME/.codex/skills/switchboard-calling" \
    "$HOME/.gemini/antigravity/skills/switchboard-calling"
  do
    mkdir -p "${SKILL_DIR}"
    cp "${TMPDIR}/SKILL.md" "${SKILL_DIR}/SKILL.md"
  done
fi

DATA_DIR="${SWITCHBOARD_DATA_DIR:-$HOME/.local/share/switchboard}"
mkdir -p "${DATA_DIR}"
printf '%s\n' "${ACTUAL}" > "${DATA_DIR}/installed_archive.sha256"

if [ -d "$HOME/.gemini/config" ]; then
  "${INSTALL_DIR}/switchboard" agy-bridge --install >/dev/null 2>&1 || true
fi

echo "Installed switchboard ${VERSION} to ${INSTALL_DIR}/switchboard"
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo "Add ${INSTALL_DIR} to your PATH:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac
