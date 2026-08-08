# nkscan

![GitHub Actions Workflow Status](https://img.shields.io/github/actions/workflow/status/activexray/nkscan/ci.yml)

A cross-platform and performant driver for Nikon film scanners.

## Support
Our goal is to support all the scanners supported by Nikon Scan, which are enumerated here by testing status.
This library doesn't have anything scanner or adapter-specific so *theoretically* it should work across devices.

- ✅ Supported, and run against real hardware
- ⚠️ Untested but theoretically should work

### Medium Format Scanners

| Scanner \ Holder | 835M | 835S | 869S  | 869G  | 869GR  | 869M | 816 | 8G1 |
|------------------|:----:|:----:|:-----:|:-----:|:------:|:----:|:---:|:---:|
| 9000             | ⚠️  |  ⚠️   | ✅   | ⚠️   |  ⚠️   |  ⚠️ | ⚠️ |  ⚠️ |
| 8000             | ⚠️  |  ⚠️   | ⚠️   | ⚠️   |  ⚠️   |  ⚠️ | ⚠️ |  ⚠️ |

### 35mm Scanners

| Scanner \ Holder | SA-21  | IA-20/21  | MA-20/21   | SA-30  | SF-210/200  |
|------------------|:------:|:---------:|:----------:|:------:|:-----------:|
| 5000             |   ⚠️   |  ⚠️      |    ⚠️     |  ⚠️    |   ⚠️       |
| 4000             |   ⚠️   |  ⚠️      |    ⚠️     |  ⚠️    |   ⚠️       |
| V                |   ⚠️   |  ⚠️      |    ⚠️     |  ⚠️    |   ⚠️       |
| IV               |   ⚠️   |  ⚠️      |    ⚠️     |  ⚠️    |   ⚠️       |

If you want to use a Firewire scanner on an old Mac that still has OS support for FireWire, let me know and I can scope it out.
It is technically possible, but getting Rust to compile a binary for older MacOS is not something I have experience in.
You could also just like, install Linux on it :)

### USB Scanners

We use [nusb](https://github.com/kevinmehall/nusb), which is a pure-Rust alternative to libusb, but it carries the same invariants.
On Windows, this means you need to associate your device with a WinUSB driver.
The most popular way to do this is with [Zadig](https://zadig.akeo.ie/).

On Linux, make sure you have the appropriate udev rules set up. Nusb has some [help](https://docs.rs/nusb/latest/nusb/#linux) on this.

MacOS *should* just work.

## Design Notes

This library is written from the ground up following the official Nikon spec of the wire protocol for the LS-5000 and LS-9000 ED scanners (located in docs/).
Comparing the two, we find an identical protocol. Some types are absent in one but not the other, some lists capabilities the other doesn't have, but all of the bits and bytes are in the same position across all the data.
This implies we don't need any model or holder specifics, we can just read what the scanner advertises as its capabilities and work from there.
This means (hopefully) we can support every scanner and every holder with a single codebase (although please test and let me know)!

The code is broken down into several layers of independent abstractions
- Transport: Defines what moving SCSI bytes is for the different OSes and physical layer (USB/FireWire)
- Protocol: An implementation of the Nikon spec via serialization and deserialization of bytes as they come off the wire. This module does no IO and is just byte-oriented.
- Session: Combines a trait object of the Transport (type erasure) with the methods from Protocol. This wraps scanner state (like global units) and provides functions that essentially perform the spec's listed actions.
- Scan: Combine the methods from Session to perform high-level scan operations. This asks the scanner what it can do and then orders session operations to do it.

## TODO

- CLI
- Python bindings

### Algorithms

The scanner occasionally asks the host to perform actions that it does not do internally.
So far this has been:
- Frame detection in certain holders (like the 869S)
- Autoexposure
- CCD anti-banding

We have not reverse-engineered with Nikon scan does and have written our own versions of these algorithms.

## License

Dual licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for
inclusion in this work by you, as defined in the Apache-2.0 license, shall be dual licensed
as above, without any additional terms or conditions.

## Related Projects and References

- [coolscanpy](https://github.com/rohanpandula/coolscanpy/)
- [Coolscan RE](https://github.com/kevihiiin/Nikon-Coolscan-RE)
- [coolscan-mods](https://github.com/kosma/coolscan-mods)
- [sane-coolscan3](http://sane-project.org/man/sane-coolscan3.5.html)
- [openICE](https://github.com/a6o/openICE)
- [digital fauxice](https://github.com/rohanpandula/digital-fauxice)
