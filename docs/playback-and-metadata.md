# Playback formats and metadata privacy

## Formats

| Format | Status |
| --- | --- |
| WAV, FLAC, MP3, Ogg Vorbis | Supported; primary validation targets |
| CDDA (CUE/BIN, redumper raw dumps) | Supported for single-session, all-audio discs |
| Opus | Supported in local `.opus`/`.ogg` files and from Collector, decoded with bundled libopus. Chained Ogg streams are not supported |
| AAC/M4A | Plays where Symphonia decodes it; gapless playback is not handled |

Stereo and mono are supported. Multichannel files are rejected until there is an
explicit channel mapper.

Gapless and bit-perfect output have not been qualified through output capture.
Successful playback does not mean the output is bit-perfect. See
[release gates](release-gates.md) for the qualification checklist.

## What is sent to MusicBrainz

Automatic metadata lookup is on by default and can be turned off in Settings.
Lookups only fill missing names; existing tags are never overwritten and source
files are never written.

Requests contain only one of:

- a disc ID, for complete all-audio CUE/redumper discs;
- a MusicBrainz release ID already embedded in the files;
- album and artist names, for an exact search.

Audio and filesystem paths are never sent. Responses are cached in the local
database, so most lookups are made only once.
