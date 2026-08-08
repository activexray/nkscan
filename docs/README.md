## Nikon-official Wire Protocols

Contained here are the original Nikon wire protocol documents for the LS-9000 and LS-50000 (thanks @kosma!).

I've used pandoc to convert them to markdown so they could by read here.

### Issues

There are several issues with the spec that needs correcting/confirming, and we'll keep track of that here.

#### LS-9000

- The DTC table (2-11-2) has 0x03 as READ/SEND but the LUT section (2-11-4) says that this unit doesn't support it.
