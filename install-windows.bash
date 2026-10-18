#!/usr/bin/env bash
# VANTA Sigil — Windows (Git Bash).
# Uso: bash install-windows.bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
THEME="$ROOT/themes/vanta-sigil"

if [[ ! -f "$THEME/package.json" ]]; then
  echo "erro: nao achei $THEME/package.json" >&2
  exit 1
fi

if command -v python >/dev/null 2>&1; then
  PY=python
elif command -v python3 >/dev/null 2>&1; then
  PY=python3
else
  echo "erro: instale Python e rode de novo" >&2
  exit 1
fi

export VANTA_THEME_SRC="$THEME"
export VANTA_OS=windows

"$PY" - <<'PY'
import json, os, shutil, sys
from pathlib import Path

THEME_LABEL = "VANTA\u3164\u3164(Sigil Theme)"
EXT_ID = "computador.vanta-sigil"
EXT_DIR_NAME = "computador.vanta-sigil-1.0.0"
LYNX_ID = "bastndev.lynx-theme"

KEYS = {
    "window.autoDetectColorScheme": False,
    "workbench.colorTheme": THEME_LABEL,
    "workbench.iconTheme": "vanta-pro-dark",
    "workbench.productIconTheme": "vanta-material-icons",
    "editor.fontFamily": "JetBrainsMono NF, Consolas, monospace",
    "editor.fontLigatures": True,
    "editor.fontSize": 14,
    "editor.lineHeight": 1.45,
    "terminal.integrated.fontFamily": "JetBrainsMono NF, Consolas, monospace",
    "terminal.integrated.fontSize": 13,
    "terminal.integrated.fontWeight": "normal",
    "terminal.integrated.fontWeightBold": "bold",
    "terminal.integrated.lineHeight": 1.2,
    "terminal.integrated.letterSpacing": 0,
    "terminal.integrated.cursorStyle": "line",
    "terminal.integrated.cursorWidth": 2,
    "terminal.integrated.cursorBlinking": True,
    "terminal.integrated.gpuAcceleration": "auto",
    "terminal.integrated.minimumContrastRatio": 1,
    "terminal.integrated.defaultProfile.windows": "PowerShell",
    "terminal.integrated.env.windows": {"CARGO_TERM_COLOR": "always", "COLORTERM": ""},
    "rust-analyzer.runnables.extraEnv": {"CARGO_TERM_COLOR": "always", "COLORTERM": ""},
}
ANSI = {
    "terminal.foreground": "#FF334A",
    "terminalCursor.foreground": "#FF0033",
    "terminal.selectionBackground": "#FF1F386b",
    "terminal.ansiBlack": "#4A0610",
    "terminal.ansiRed": "#E1062C",
    "terminal.ansiGreen": "#FF1F38",
    "terminal.ansiYellow": "#FF4D5A",
    "terminal.ansiBlue": "#C40024",
    "terminal.ansiMagenta": "#FF2D4A",
    "terminal.ansiCyan": "#FF6B74",
    "terminal.ansiWhite": "#FF8A94",
    "terminal.ansiBrightBlack": "#8E1020",
    "terminal.ansiBrightRed": "#FF0033",
    "terminal.ansiBrightGreen": "#FF2A40",
    "terminal.ansiBrightYellow": "#FF5C66",
    "terminal.ansiBrightBlue": "#FF1744",
    "terminal.ansiBrightMagenta": "#FF4060",
    "terminal.ansiBrightCyan": "#FF7A84",
    "terminal.ansiBrightWhite": "#FFB4BA",
}

def load(path, default):
    if not path.is_file() or not path.read_text(encoding="utf-8").strip():
        return default
    return json.loads(path.read_text(encoding="utf-8"))

def dump(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=4, ensure_ascii=False) + "\n", encoding="utf-8")

home = Path.home()
appdata = os.environ.get("APPDATA") or str(home / "AppData" / "Roaming")
user = Path(appdata) / "Cursor" / "User"
exts = home / ".cursor" / "extensions"
src = Path(os.environ["VANTA_THEME_SRC"])
dest = exts / EXT_DIR_NAME
if dest.exists():
    shutil.rmtree(dest)
shutil.copytree(src, dest, ignore=shutil.ignore_patterns("__pycache__", ".git", "install.sh", "install.py"))

catalog = exts / "extensions.json"
entries = load(catalog, [])
if not isinstance(entries, list):
    entries = []
loc = dest.resolve()
found = False
for e in entries:
    if not isinstance(e, dict):
        continue
    eid = (e.get("identifier") or {}).get("id")
    if eid == LYNX_ID:
        e["disabled"] = True
    if eid == EXT_ID:
        found = True
        e["version"] = "1.0.0"
        e["disabled"] = False
        e["relativeLocation"] = EXT_DIR_NAME
        e["location"] = {"$mid": 1, "path": "/" + loc.as_posix(), "fsPath": str(loc), "scheme": "file"}
if not found:
    entries.append({
        "identifier": {"id": EXT_ID},
        "version": "1.0.0",
        "disabled": False,
        "relativeLocation": EXT_DIR_NAME,
        "location": {"$mid": 1, "path": "/" + loc.as_posix(), "fsPath": str(loc), "scheme": "file"},
    })
dump(catalog, entries)

settings = user / "settings.json"
data = load(settings, {})
if not isinstance(data, dict):
    data = {}
data.update(KEYS)
colors = data.get("workbench.colorCustomizations")
if not isinstance(colors, dict):
    colors = {}
colors.update(ANSI)
data["workbench.colorCustomizations"] = colors
dump(settings, data)
print("tema:", dest)
print("settings:", settings)
print("Lynx da loja desativado.")
print("Recarregue: Ctrl+Shift+P -> Developer: Reload Window")
PY
