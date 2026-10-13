`silence.mp3` — 0.2 seconds of generated silence, 44100 Hz, mono, 64 kbps.
Generated locally using libmp3lame; no music recordings were used.

`silence.flac` — 0.05 seconds of generated silence, 44100 Hz, mono, 16-bit.
Generated with `sox -n -r 44100 -c 1 -b 16 silence.flac trim 0 0.05`.
`silence-flac.m4a` contains the same FLAC frame in a minimal MP4 container
constructed locally, with metadata after the audio. The test also inserts
a large `free` block and encrypts the file to check reads across HTTP ranges.

Synthetic 440 Hz tones test playback before a complete download:

- `tone.mp3` — 0.05 seconds, 44100 Hz, mono, 320 kbps; repeated 2048 times in the test.
- `tone.flac` — 60 seconds, 44100 Hz, mono, 16-bit, without dithering.
- `tone-flac.m4a` — one FLAC frame of 2205 samples in MP4 with fixed packet sizes;
  the test builds a long MP4 from 4096 packets and places `moov` at the beginning
  or end, updating the tables and duration.

The source tones were generated locally using SoX:

```sh
sox -n -r 44100 -c 1 -C 320 tone.mp3 synth 0.05 sine 440 vol 0.2
sox -D -n -r 44100 -c 1 -b 16 tone.flac synth 60 sine 440 vol 0.2
```

`seek.flac` and `seek.mp3` test seek positions with a 60-second tone whose
frequency sweeps from 80 to 2000 Hz. The MP3 is encoded at 320 kbps. The test
compares PCM for FLAC/MP4 and frequency for approximate MP3 seeking, and checks
HTTP Range transfer counts for forward and backward seeks.

```sh
sox -D -n -r 44100 -c 1 -b 16 seek.flac synth 60 sine 80-2000 vol 0.2
sox seek.flac -C 320 seek.mp3
```

These fixtures are used only in tests and are not included in the release player.
