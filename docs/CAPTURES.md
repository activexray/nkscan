# Reading the NikonScan capture corpus

The reference captures live in
`/mnt/storage/NikonScanDecomp/scan_captures/`. Each is a `scsi_trace.bin`
written by the NKDSBP2 Windows proxy, one SREC record per IOCTL_SCSISCAN_CMD
exchange. `examples/trace.rs` decodes one into text a session's own
`RUST_LOG=nkscan::cdb=trace` output can be diffed against.

## Running the decoder

```text
cargo run --example trace -- /mnt/storage/NikonScanDecomp/scan_captures/another_normal_scan_of_one_frame/scsi_trace.bin
```

The leading `[n]` on each line is the record's position in the exchange, so a
NikonScan capture and an `nkscan` run can be diffed in order. Image-data READs
are summed rather than dumped; one scan is hundreds of megabytes and adds
nothing to a movement diff. The tail line reports the total:

```text
image data: 1150303176 bytes across 3043 READs
```

## What the decoder shows

Record type is recognized from the CDB: INQUIRY, TEST UNIT READY, SET/GET
WINDOW, SCAN, SET/GET PARAMETER, READ [..], SEND [BOUNDARY], and so on. The two
that matter for the movement story:

- **SET WINDOW** is printed in full. Id, resolution, origin, size, and the
  descriptor flags. Because SET WINDOW *is* the move command on this family
  (`docs/PROTOCOL.md`). A capture's window origins are its stage movements.
- **FRAME BOUNDARY** (88h) decodes the rectangles the unit holds, one line per
  frame (`rect top=.. left=.. bottom=.. right=..`), so a capture shows whether
  the unit has measured the strip or is still holding a whole-sensor default.

## The reference capture: `another_normal_scan_of_one_frame`

LS-5000, 6x9 strip, one frame scanned. 3491 records, ~1.15 GB. The stage story:

| # | Exchange | Meaning |
|---|----------|---------|
| 23-28 | GET WINDOW, MODE SELECT, INQUIRY | Preamble: read what the last session left |
| 118-121 | SET WINDOW `(0,0)/(8964,13176)` 4000dpi | Back to a whole-sensor window |
| 130-131 | FRAME BOUNDARY read | Unit holds one whole-sensor rect `(0,0,13859,9999)`: the strip has never been measured |
| 132, 138 | SEND [BOUNDARY] (68 then 52 bytes) | Host computes and writes frame rects |
| 156-158 | SET WINDOW `(518,0)/(8964,34644)` 83dpi, THUMBNAIL | Thumbnail pass over the whole strip, origin at the strip's left edge |
| 160-163 | SCAN + COOPERATION reads | The thumbnail image |
| 173-175 | GET WINDOW | Read back the thumbnail descriptors |
| 178 | SEND [BOUNDARY] (52 bytes) | Final measured boundary, three frames |
| 210-211 | SET PARAMETER `A0h` + EXECUTE | Autofocus |
| 273-276 | SET WINDOW `(518,12720)/(8964,8964)` 666dpi | Prescan / AE window at frame 2's front edge |
| 319-322 | SET WINDOW `(518,12720)/(8964,8964)` 666dpi, multi-reading | Second metering pass |
| 384-387 | SET WINDOW `(518,12720)/(8964,8964)` 4000dpi, multi-reading | The scan itself |
| 389-396 | SCAN + COOPERATION reads | Final image |
| 3438-3441 | SET WINDOW `(0,0)/(8964,13176)` 4000dpi | Teardown back to a whole-sensor window |

The scan window sits at the **front edge** of the chosen frame: origin
`(518, 12720)`, size `(8964, 8964)`. Every pass. Thumbnail, prescan, scan --
uses that same frame origin, so the stage moves once, to the frame, and stays
there. A run that "moves the whole holder forwards and backwards" is one whose
window origin lands at the far end of the strip instead of at a frame edge.
