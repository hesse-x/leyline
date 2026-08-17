# Unicode bidi fixtures

- Unicode version: 16.0.0
- `unicode-bidi`: 0.3.18 (`UNICODE_VERSION == (16, 0, 0)`)
- `BidiTest.txt` SHA-256: `93e5eb9d88ca89dcf895f5576486a3363762ad2aa8f2db2fa56fe60cb82b9520`
- `BidiCharacterTest.txt` SHA-256: `d04a51a90052dcd71c4e91ee5b3a9d973ee35c12406b5a99875ac8163c8f2804`
- Source: Unicode Character Database 16.0.0
- License/terms: retained in the headers of both fixture files.

The scalar conformance runner consumes the files directly. Terminal cell projection tests remain
separate because wide cells, spacers, and transparent X9 placeholders are Leyline policy rather
than UAX #9 conformance behavior.
