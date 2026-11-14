# SICK binary scan fixture provenance

The `EMULATOR` test constant in `src/timeseries/sick_cola/binary.rs` retains the original 3553-byte telegram from SICKAG's `sick_scan_xd/test/emulator/test_server.py`, revision `a4a2b7330128a74850b4e98c36435c9b893c0651`. Only the textual representation changes from colon-separated hex to plain hex; all wire bytes are unchanged. SHA256: `1f270eaceb947d78cbbe1e6c60a3bf35bedbc59c9321ac827d09f5ccedebe8fc`.

Upstream source: https://github.com/SICKAG/sick_scan_xd/blob/a4a2b7330128a74850b4e98c36435c9b893c0651/test/emulator/test_server.py

The repository's Apache 2.0 license is retained in `LICENSE.sick_scan_xd`. The upstream script has no separate per-file copyright notice; no upstream NOTICE file is present at this revision. No upstream implementation code is copied into the adapter. Capturefab's decoder and specification-derived fixtures are independently written.

This original telegram is `sSN`, so it validates the common binary scan body and is rejected by the single-query reader. It is not relabeled as a captured `sRA` response. The unchanged printed `sRA` example from SICK 8014631/1PIK/2023-02-21 p111 has incorrect checksum 2B (actual XOR CB) and is retained separately as a negative fixture. Positive `sRA` fixtures are explicitly specification-derived.

## Published LMS CoLa A read transcript

`lms-published-sra.bin` contains the exact215-byte ASCII reply represented by
SICK LMS1xx8012471/1U1F/2026-02-18 Table127 printed/physical126 and
LMS5xx8013796/1NJF/2024-09-27 Table158 printed/physical162. Both original
PDF pages were rendered and visually checked. Only the printed STX/ETX/space
macros are materialized as their denoted bytes; all values/counts/tails are
unchanged. SHA256: `ca81f93e9f85e0164076d8459e9c50324b07ea22bbcef1d3ca7838bcb4acf95d`.

Official sources:
- https://www.sick.com/media/docs/1/31/331/operating_instructions_lms1xx_2d_lidar_sensor_en_im0031331.pdf
- https://www.sick.com/media/docs/4/14/514/operating_instructions_lms5xx_2d_lidar_sensors_en_im0037514.pdf

The ASCII examples are valid21-point/final-six-zero read replies. An indexed
web extraction inserted an extra8A8 and omitted two tail fields; that corrupted
extraction is not retained or attributed as the original PDF. These functional
protocol bytes are a published transcript replay, not a physical-device test.
The same pages' separate CoLa B checksum discrepancy remains independent.
No manufacturer implementation or SDK code is copied.
