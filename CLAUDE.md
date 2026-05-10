# Claude Notes

## README Images

The README screenshots are generated assets, not manual desktop captures.

Use this exact command from the repository root:

```bash
make readme-images
```

That runs `python3 scripts/render-readme-images.py` and rewrites:

- `docs/images/boot-dashboard.png`
- `docs/images/work-cards.png`

The renderer is a Pillow-based synthetic terminal composition. Do not use VHS,
freeze, silicon, or ad hoc interactive screenshots for these README images
unless the renderer itself is being replaced in the same change.

If Pillow is missing:

```bash
python3 -m pip install --user pillow
```

After changing splash copy, WorkCard copy, colors, role names, or release
version, run `make readme-images`, inspect both PNGs, and commit the script/doc
changes together with the regenerated images.
