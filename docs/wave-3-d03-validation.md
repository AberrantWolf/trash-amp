# Wave 3 shared-audio migration validation

## Implemented locally (2026-09-10)

- PCM WAV files are opened through `junk-libs-audio` opaque byte sources and its
  exact integer-frame decoder. CUE and redumper tracks use the shared opened-reader
  adapters. Filesystem discovery and sidecar selection remain Trash Amp policy.
- Rate conversion uses the shared bounded streaming sinc service. Same-rate audio
  bypasses resampling. Existing exact-duration vectors include one-frame, partial,
  full-block, and multi-block sources.
- Transport commands cross into the worker as integer output frames. Replace,
  seek, next, previous, and truncate are validated by the shared generic queue
  policy before Trash Amp applies them. The CPAL callback remains a product adapter
  containing only bounded ring-buffer reads, atomics, channel mapping, and device
  sample conversion.
- Existing product vectors still cover exact WAV samples and seek, uninterrupted
  two-track output, CUE track boundaries, repeated raw/CUE seeks, raw quality
  rejection, and resampling length.

## Not implemented

- Trash Amp's pre-existing compressed-format decoder remains product-local until
  independent codec vectors qualify migration. It is not presented as shared
  `junk-libs-audio` support.
- Output-device discovery/reopen policy remains in Trash Amp. No new DSP UI or
  effects were added.

## Externally unqualified

- Physical CPAL playback, loopback gaplessness, compressed-album gaplessness, and
  real redumper dump playback still require their documented hardware/private
  fixtures. The automated audio tests require no GUI or audio device.
