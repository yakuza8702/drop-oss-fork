#!/usr/bin/env bash
# Restructure a Drop library so versions are archives instead of folders
# containing an archive.
#
#   BEFORE (what the client sees: just data.7z, a few chunks)
#     GameName/v1.0/data.7z
#     GameName/v1.0/game.exe
#
#   AFTER  (what the client sees: the real game files, correct size)
#     GameName/v1.0.7z
#
# Safe by default: prints the plan and changes nothing. Pass --apply to do it.
#
#   ./fix-library-layout.sh /srv/.../00-games/drop-library            # dry run
#   ./fix-library-layout.sh /srv/.../00-games/drop-library --apply    # do it
set -uo pipefail

LIB="${1:-}"
APPLY=0
[ "${2:-}" = "--apply" ] && APPLY=1

if [ -z "$LIB" ] || [ ! -d "$LIB" ]; then
  echo "usage: $0 <library-path> [--apply]" >&2
  echo "  e.g. $0 /srv/mergerfs/mergerfs_data_user/00-games/drop-library" >&2
  exit 1
fi

# archives Drop can import as a version (libraries/droplet/src/versions/mod.rs)
EXTS="tar pax cpio zip jar ar xar rar rpm 7z iso"

is_archive() {
  local f="$1" ext="${1##*.}"
  for e in $EXTS; do [ "$ext" = "$e" ] && return 0; done
  return 1
}

plan() { printf '%s\n' "$*"; }

changed=0
skipped=0

# game folder -> version entries
for game in "$LIB"/*/; do
  [ -d "$game" ] || continue
  game="${game%/}"
  for ver in "$game"/*; do
    [ -e "$ver" ] || continue

    # already an archive version: nothing to do
    if [ -f "$ver" ] && is_archive "$ver"; then
      continue
    fi
    [ -d "$ver" ] || continue

    # collect archives inside the version folder
    archives=()
    while IFS= read -r -d '' f; do
      is_archive "$f" && archives+=("$f")
    done < <(find "$ver" -maxdepth 1 -type f -print0 | sort -z)

    if [ "${#archives[@]}" -ne 1 ]; then
      if [ "${#archives[@]}" -gt 1 ]; then
        plan "SKIP   $ver  (contains ${#archives[@]} archives - ambiguous)"
      fi
      skipped=$((skipped + 1))
      continue
    fi

    archive="${archives[0]}"
    target="$game/$(basename "$ver").${archive##*.}"

    # what else is in the folder besides the archive?
    extras=$(find "$ver" -maxdepth 1 -type f ! -path "$archive" -printf '%f ' 2>/dev/null)

    if [ -e "$target" ]; then
      plan "SKIP   $ver  ($target already exists)"
      skipped=$((skipped + 1))
      continue
    fi

    plan "CONVERT $ver"
    plan "        archive : $(basename "$archive")  ($(du -h "$archive" | cut -f1))"
    plan "        becomes : $target"
    if [ -n "$extras" ]; then
      plan "        extras  : $extras  <- these live OUTSIDE the archive and will NOT be"
      plan "                  part of the version. Merge them into the archive first, or"
      plan "                  delete them if they are redundant (e.g. a helper 7zr.exe)."
    fi
    changed=$((changed + 1))

    if [ "$APPLY" = "1" ]; then
      mv -f "$archive" "$target" || { plan "        ERROR: mv failed"; continue; }
      # keep the old folder only if it still holds something (extras)
      if [ -z "$(find "$ver" -maxdepth 1 -type f -print -quit)" ]; then
        rmdir "$ver" 2>/dev/null || true
      fi
      plan "        done"
    fi
  done
done

echo
if [ "$APPLY" = "1" ]; then
  echo "Applied: $changed version(s) converted, $skipped left alone."
  echo "Now re-import each converted version in the Drop admin UI (the manifest is"
  echo "stored in the database, so an old one keeps serving the old file list)."
else
  echo "Dry run: $changed version(s) would be converted, $skipped left alone."
  echo "Re-run with --apply to do it."
fi
