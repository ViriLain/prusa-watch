# Vision reference corpus

`field-1.png`, `field-2.png`, and `field-3.png` are lossless conversions of the
repository owner's existing annotated job-419 captures:

- `20260928-100312_f00089_ok_p0.51.jpg`
- `20260928-100549_f00093_failure_p1.81.jpg`
- `20260928-100630_f00097_failure_p0.00.jpg`

`pattern-*.png` are generated RGB patterns at an odd aspect ratio, an upscale size
and an exact 2x downscale size. These inputs exercise pixel/inference parity;
they have no clean/failure accuracy labels. The field pictures already contain
annotation, so they must not be treated as raw detector-quality evidence.

`golden.json` records source hashes, detections and summed confidence from the
independent Python detector. `metadata.json` records the model SHA and reference
runtime versions. `scripts/reference_detector.py` is the original project Python
implementation from `ae6415734c0bde25924637bc416b35d4a42dd1d3`, with resize changed
to OpenCV `INTER_LINEAR_EXACT`. Original Obico AGPL attribution is retained.
The corpus and reference source follow the project's AGPL-3.0-or-later license.

To regenerate in an isolated environment:

```sh
python3 -m venv /tmp/prusa-watch-reference
/tmp/prusa-watch-reference/bin/pip install -r tests/vision-requirements.txt
/tmp/prusa-watch-reference/bin/python scripts/generate_vision_fixtures.py models/model-weights.onnx
```

Regeneration also writes independent byte hashes for six resize dimensions in
`resize_opencv.json`. Review input and model hashes when changing goldens;
changing a fixture to match Rust without an independent reference invalidates it.
The Rust comparison allows 0.00001 confidence and 0.001 pixel box differences.
