#!/usr/bin/env bash

# Detect available clipboard command (macOS, Linux Wayland/X11, WSL)
if command -v pbcopy &>/dev/null; then
  CLIP_CMD="pbcopy"
elif command -v wl-copy &>/dev/null; then
  CLIP_CMD="wl-copy"
elif command -v xclip &>/dev/null; then
  CLIP_CMD="xclip -selection clipboard"
elif command -v clip.exe &>/dev/null; then
  CLIP_CMD="clip.exe"
else
  echo "Error: No supported clipboard tool found (pbcopy, wl-copy, xclip, or clip.exe)." >&2
  exit 1
fi

# Ensure execution inside a Git repository
if ! git rev-parse --is-inside-work-tree &>/dev/null; then
  echo "Error: Not inside a Git repository." >&2
  exit 1
fi

# Process tracked Rust files safely (handles spaces/special characters)
git ls-files '*.rs' -z | while IFS= read -r -d '' file; do
  echo "// path: $file"
  cat "$file"
  echo -e "\n" # Spacing between files
done | $CLIP_CMD

echo "Successfully copied tracked .rs files to clipboard!"
