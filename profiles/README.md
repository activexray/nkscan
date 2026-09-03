# Scanner profiles

Nikon Scan's own input profiles, one per scanner per film type, converted for
this crate by `scripts/profiles.py`. Run that from the repo root over a copy of
the installer's `Profiles` directory to rebuild them.

Two changes were made to each:

- The profile class was `nkpf`, a Nikon private class that we convert to `scnr`.
- Converted to accept linear instead of gamma 2.2.

Nothing else was touched. The measurements are Nikon's.

## What are the files

`_P` positive, `_N` negative, `_K` Kodachrome, `_MN` monochrome negative.
The LS-8000 and LS-4000 have no `_MN`.

Not here:

- The unsuffixed profile of each family, whose table is byte-identical to `_P`.
- `NKLS50_*`, whose measurement tags are byte-identical to `NKLS5000_*` and
  differ only in the description string. `scan::profile` maps the LS-50 onto the
  LS-5000 files.
- `_R`, which is the same bytes for every model and older than the rest, so it
  characterizes no particular scanner. The Nikon Scan manual's "Scanner RGB"
  color space is what it reads like.
- Every working space and monitor profile in that directory, which describe
  output rather than the scanner.

## Provenance

Derived from the ICM profiles in the Nikon Scan 4 installer, which carry the
notice "Nikon Inc. & Nikon Corporation 2003". That notice is retained in each
file.

These files are not covered by this crate's MIT/Apache-2.0 license, and no license to them is granted here.

The tables have been altered as described above, so these are not what Nikon shipped and should not be read as Nikon's characterization.
Rebuild from your own installer copy with `scripts/profiles.py` for the originals.
