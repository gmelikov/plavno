"""Regenerate synthetic tagged fixtures using SoX and mutagen==1.47.0."""

import base64
import shutil
import struct
import subprocess
import zlib
from pathlib import Path

from mutagen.flac import FLAC, Picture
from mutagen.id3 import ID3, APIC, TIT2, TPE1, TCON
from mutagen.mp4 import MP4, MP4Cover
from mutagen.oggvorbis import OggVorbis

ROOT = Path(__file__).resolve().parent
TITLE = "Тестовый трек"
ARTIST = "Исполнитель"
GENRE = "Instrumental"


def png(color):
    def chunk(kind, data):
        return (
            struct.pack(">I", len(data))
            + kind
            + data
            + struct.pack(">I", zlib.crc32(kind + data))
        )

    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", 4, 4, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress((b"\0" + bytes(color) * 4) * 4))
        + chunk(b"IEND", b"")
    )


def picture(kind, color):
    result = Picture()
    result.type = kind
    result.mime = "image/png"
    result.width = result.height = 4
    result.depth = 24
    result.data = png(color)
    return result


front = picture(3, (20, 40, 60))
back = picture(4, (200, 0, 0))
for extension, source in [("flac", "silence.flac"), ("mp3", "silence.mp3"),
                          ("m4a", "silence-flac.m4a")]:
    shutil.copyfile(ROOT / source, ROOT / f"tagged.{extension}")

flac = FLAC(ROOT / "tagged.flac")
flac["title"], flac["artist"], flac["genre"] = TITLE, ARTIST, GENRE
flac.add_picture(back)
flac.add_picture(front)
flac.save(padding=lambda _: 0)

id3 = ID3()
id3.add(TIT2(encoding=3, text=TITLE))
id3.add(TPE1(encoding=3, text=ARTIST))
id3.add(TCON(encoding=3, text=GENRE))
id3.add(APIC(encoding=3, mime="image/png", type=3, data=front.data))
id3.save(ROOT / "tagged.mp3", v2_version=3, padding=lambda _: 0)

mp4 = MP4(ROOT / "tagged.m4a")
mp4["\xa9nam"], mp4["\xa9ART"], mp4["\xa9gen"] = [TITLE], [ARTIST], [GENRE]
mp4["covr"] = [MP4Cover(front.data, imageformat=MP4Cover.FORMAT_PNG)]
mp4.save(padding=lambda _: 0)

subprocess.run(["sox", str(ROOT / "silence.flac"), str(ROOT / "tagged.ogg")], check=True)
ogg = OggVorbis(ROOT / "tagged.ogg")
ogg["title"], ogg["artist"], ogg["genre"] = TITLE, ARTIST, GENRE
ogg["metadata_block_picture"] = base64.b64encode(front.write()).decode("ascii")
ogg.save(padding=lambda _: 0)

shutil.copyfile(ROOT / "silence.flac", ROOT / "tagged-partial.flac")
partial = FLAC(ROOT / "tagged-partial.flac")
partial["title"] = " \t "
partial["albumartist"] = "Исполнитель альбома"
partial.save(padding=lambda _: 0)

legacy = b"TAG" + b"Legacy title".ljust(30, b"\0") + b"Legacy artist".ljust(30, b"\0") + bytes(65)
shutil.copyfile(ROOT / "silence.mp3", ROOT / "tagged-v1.mp3")
for name in ["tagged.mp3", "tagged-v1.mp3"]:
    with (ROOT / name).open("ab") as output:
        output.write(legacy)
