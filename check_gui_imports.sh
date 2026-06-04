#!/bin/bash
# Lint: ensure src/gui/ only uses rustxwidgets abstractions, never raw backend crates.
set -euo pipefail
cd "$(dirname "$0")"

gui_dir="src/gui"
banned_crates='pancurses|gtk(?:_dynamic_loader)?|crossterm|ratatui'
errors=0

echo "=== Check 1: No raw backend crate imports in src/gui/ ==="

while IFS= read -r -d '' f; do
    results=$(rg -n \
        -e '^\s*(use\s+|extern\s+crate\s+)('"$banned_crates"')[\s;:{]' \
        -e '(?<![:\w.])('"$banned_crates"')::' \
        "$f" 2>/dev/null || true)
    if [[ -n "$results" ]]; then
        echo "ERROR: $f contains banned backend crate reference:"
        echo "$results"
        errors=1
    fi
done < <(find "$gui_dir" -name '*.rs' -print0)

if [[ $errors -eq 0 ]]; then
    echo "  PASS: No raw backend crate imports found."
fi

echo ""
echo "=== Check 2: No concrete rustxwidgets::App / rustxwidgets::Window in shared files ==="

shared_files=(
    "$gui_dir/clipboard.rs"
    "$gui_dir/dialogs.rs"
    "$gui_dir/edit.rs"
    "$gui_dir/keymap.rs"
    "$gui_dir/menu.rs"
    "$gui_dir/mod.rs"
    "$gui_dir/sheet.rs"
)

for f in "${shared_files[@]}"; do
    if [[ ! -f "$f" ]]; then
        continue
    fi
    results=$(rg -n \
        -e 'rustxwidgets::(App|Window)' \
        "$f" 2>/dev/null || true)
    if [[ -n "$results" ]]; then
        echo "WARNING: $f references concrete rustxwidgets type:"
        echo "$results"
        errors=1
    fi
done

if [[ $errors -eq 1 ]]; then
    echo ""
    echo "FAIL: Backend abstractions violated."
else
    echo "  PASS: No concrete rustxwidgets types in shared files."
    echo ""
    echo "OK: src/gui/ uses only rustxwidgets abstractions."
fi
exit $errors
