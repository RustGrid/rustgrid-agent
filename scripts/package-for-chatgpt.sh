#!/usr/bin/env bash
set -euo pipefail

usage() {
    echo "usage: $0 [output.tar.gz]" >&2
}

if [ "$#" -gt 1 ]; then
    usage
    exit 2
fi

repo_root="$(git rev-parse --show-toplevel 2>/dev/null)" || {
    echo "error: run this script from inside the rustgrid-agent repository" >&2
    exit 1
}
cd "$repo_root"

short_sha="$(git rev-parse --short=12 HEAD)"
output="${1:-/tmp/rustgrid-agent-chatgpt-${short_sha}.tar.gz}"
case "$output" in
    /*) ;;
    *) output="$repo_root/$output" ;;
esac

if [ -e "$output" ]; then
    echo "error: output already exists: $output" >&2
    exit 1
fi

bash scripts/check-secrets.sh

staging_dir="$(mktemp -d /tmp/rustgrid-agent-chatgpt.XXXXXX)"
archive_root="$staging_dir/rustgrid-agent"
excluded_list="$staging_dir/excluded-files.txt"
mkdir -p "$archive_root"
touch "$excluded_list"

cleanup() {
    rm -rf "$staging_dir"
}
trap cleanup EXIT

git ls-files -z | while IFS= read -r -d '' path; do
    basename="${path##*/}"

    case "$basename" in
        .env|.env.*|*.env|*.env.*|.envrc|.dev.vars|*.tfvars|*.tfvars.json|\
        .npmrc|.pypirc|.netrc|credentials.json|secrets.json|\
        .rustgrid-agent.json|.rustgrid-agent.json.credentials|id_rsa|id_ed25519|\
        *.pem|*.key|*.p12|*.pfx)
            printf '%s\n' "$path" >>"$excluded_list"
            continue
            ;;
    esac

    case "$path" in
        .direnv/*|deploy/credentials/*|target/*|debug/*)
            printf '%s\n' "$path" >>"$excluded_list"
            continue
            ;;
    esac

    # A tracked file can be deleted in the working tree. Do not resurrect its
    # index version in an archive intended to represent the current checkout.
    if [ ! -e "$path" ] && [ ! -L "$path" ]; then
        continue
    fi

    destination="$archive_root/$path"
    mkdir -p "$(dirname "$destination")"
    cp -Pp -- "$path" "$destination"
done

mkdir -p "$(dirname "$output")"
tar -C "$staging_dir" -czf "$output" rustgrid-agent

if command -v sha256sum >/dev/null 2>&1; then
    checksum="$(sha256sum "$output" | awk '{print $1}')"
else
    checksum="$(shasum -a 256 "$output" | awk '{print $1}')"
fi

tracked_count="$(find "$archive_root" -type f -o -type l | wc -l | tr -d ' ')"
excluded_count="$(wc -l <"$excluded_list" | tr -d ' ')"
untracked_count="$(git ls-files --others --exclude-standard | wc -l | tr -d ' ')"

printf 'Created: %s\n' "$output"
printf 'SHA-256: %s\n' "$checksum"
printf 'Included tracked files: %s\n' "$tracked_count"
printf 'Excluded sensitive tracked files: %s\n' "$excluded_count"
printf 'Excluded untracked files: %s\n' "$untracked_count"
