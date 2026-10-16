#!/usr/bin/env python3
"""Create the HACS zip with manifest.json directly at archive root."""
import json
from pathlib import Path
import zipfile

root = Path(__file__).resolve().parents[1]
component = root / 'custom_components/hifiberry_tts'
output = root / 'dist/hifiberry_tts.zip'
output.parent.mkdir(exist_ok=True)
manifest = json.loads((component / 'manifest.json').read_text())
assert manifest['domain'] == 'hifiberry_tts'
assert (component / 'brand/icon.png').is_file()
with zipfile.ZipFile(output, 'w', zipfile.ZIP_DEFLATED) as archive:
    for path in sorted(component.rglob('*')):
        if path.is_file() and '__pycache__' not in path.parts and path.suffix != '.pyc':
            archive.write(path, path.relative_to(component))
with zipfile.ZipFile(output) as archive:
    assert 'manifest.json' in archive.namelist()
    assert archive.testzip() is None
print(f'{output}: v{manifest["version"]}')
