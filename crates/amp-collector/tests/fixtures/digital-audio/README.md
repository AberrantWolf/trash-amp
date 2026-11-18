# Digital purchase acceptance fixtures

Small generated audio and artwork from Collector's HL-02 acceptance corpus,
retained here so native tests and the `ui_fixture` example require no sibling
checkout. `01.flac` and `02.mp3` exercise native formats, embedded descriptive
metadata, playback and independent download identities; `cover.png` exercises
retained artwork. They contain synthetic signals, not commercial recordings.
Independent codec/PCM vectors live in the pinned junk-libs audio test suite.

The Original Ogg playback test uses 250 ms stereo 440 Hz fixtures. Generate each
with FFmpeg using `-f lavfi -i sine=frequency=440:duration=0.25 -fflags +bitexact`
and `-strict -2 -c:a vorbis -ac 2` for Vorbis or `-c:a libopus -ac 2` for Opus.
Both inputs are presented with the same generic `audio/ogg` type.
