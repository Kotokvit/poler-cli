#!/usr/bin/env bash
# POLER CLI — Sovereign Universal One-Line Installer
# Usage: curl -fsSL https://raw.githubusercontent.com/Kotokvit/poler-cli/main/install.sh | bash

set -e

REPO="Kotokvit/poler-cli"
VERSION="v0.100.0"

BOLD="$(tput bold 2>/dev/null || printf '')"
GREEN="$(tput setaf 2 2>/dev/null || printf '')"
CYAN="$(tput setaf 6 2>/dev/null || printf '')"
RED="$(tput setaf 1 2>/dev/null || printf '')"
RESET="$(tput sgr0 2>/dev/null || printf '')"

printf "%s\n" "${CYAN}${BOLD}⚡ Installing poler (v0.100.0) — Sovereign Ultra-Fast CLI Suite...${RESET}"

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Linux)
    case "$ARCH" in
      x86_64)
        ASSET="poler-linux-x86_64"
        EXPECTED_SHA="cd27a51b2a8aaf7575871160cb4fc410f53b757a1d3d76dcbb8d781547524c8f"
        ;;
      *)
        printf "%s\n" "${RED}Error: Unsupported architecture $ARCH on Linux.${RESET}" >&2
        printf "Prebuilt binaries currently support x86_64. You can build from source via 'cargo install poler-cli'.\n"
        exit 1
        ;;
    esac
    ;;
  Darwin)
    case "$ARCH" in
      arm64|aarch64)
        ASSET="poler-macos-arm64"
        EXPECTED_SHA="e7133802131f0b5008d56b40cdfd52599f676e0a1f89342e8b509fae9081d157"
        ;;
      *)
        printf "%s\n" "${RED}Error: Intel macOS (x86_64) prebuilt not packaged for v0.100.0.${RESET}" >&2
        printf "Please build via 'cargo install poler-cli'.\n"
        exit 1
        ;;
    esac
    ;;
  *)
    printf "%s\n" "${RED}Error: Unsupported operating system $OS.${RESET}" >&2
    printf "For Windows, download poler-windows-x86_64.exe from: https://github.com/$REPO/releases\n"
    exit 1
    ;;
esac

DOWNLOAD_URL="https://github.com/$REPO/releases/download/$VERSION/$ASSET"

TMP_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t 'poler_install')"
cleanup() { rm -rf "$TMP_DIR"; }
trap cleanup EXIT

TARGET_FILE="$TMP_DIR/poler"

printf "Downloading %s from %s...\n" "$ASSET" "$REPO"
if command -v curl >/dev/null 2>&1; then
  curl -fsSL "$DOWNLOAD_URL" -o "$TARGET_FILE"
elif command -v wget >/dev/null 2>&1; then
  wget -qO "$TARGET_FILE" "$DOWNLOAD_URL"
else
  printf "%s\n" "${RED}Error: Neither curl nor wget was found on the system.${RESET}" >&2
  exit 1
fi

# Verify Checksum
if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL_SHA="$(sha256sum "$TARGET_FILE" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL_SHA="$(shasum -a 256 "$TARGET_FILE" | awk '{print $1}')"
else
  ACTUAL_SHA=""
fi

if [ -n "$ACTUAL_SHA" ] && [ "$ACTUAL_SHA" != "$EXPECTED_SHA" ]; then
  printf "%s\n" "${RED}Error: SHA256 verification failed!${RESET}" >&2
  printf "Expected: %s\nActual:   %s\n" "$EXPECTED_SHA" "$ACTUAL_SHA" >&2
  exit 1
fi

chmod +x "$TARGET_FILE"

# Determine install location
if [ -w "/usr/local/bin" ]; then
  INSTALL_DIR="/usr/local/bin"
  USE_SUDO=""
elif [ "$EUID" -eq 0 ]; then
  INSTALL_DIR="/usr/local/bin"
  USE_SUDO=""
elif command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
  INSTALL_DIR="/usr/local/bin"
  USE_SUDO="sudo"
else
  INSTALL_DIR="$HOME/.local/bin"
  mkdir -p "$INSTALL_DIR"
  USE_SUDO=""
fi

printf "Installing to %s/poler...\n" "$INSTALL_DIR"
if [ -n "$USE_SUDO" ]; then
  sudo mv "$TARGET_FILE" "$INSTALL_DIR/poler"
else
  mv "$TARGET_FILE" "$INSTALL_DIR/poler"
fi

printf "\n%s\n" "${GREEN}${BOLD}✔ Successfully installed poler to $INSTALL_DIR/poler!${RESET}"
printf "%s\n" "Run '${BOLD}poler --help${RESET}' or '${BOLD}poler mcp${RESET}' to get started."

if ! command -v poler >/dev/null 2>&1; then
  if [ "$INSTALL_DIR" = "$HOME/.local/bin" ]; then
    printf "\n%s\n" "${CYAN}Note: Make sure $HOME/.local/bin is in your PATH:${RESET}"
    printf "  export PATH=\"\$HOME/.local/bin:\$PATH\"\n"
  fi
fi
