# x3ctl

Configure an Attack Shark X3 mouse from Linux: DPI stages, polling rate,
lift-off distance, ripple control, angle snap, motion sync, sleep timers,
debounce, and battery level.

It is a userspace driver. The kernel's HID driver already makes the X3 work as
a mouse; what is missing on Linux is the vendor's configuration software, and
that is what this replaces. It talks to the mouse through `/dev/hidraw*` using
raw `ioctl`/`poll` calls and has no dependencies beyond the Rust standard
library.

Both the cable (`1d57:fa61`) and the 2.4G dongle (`1d57:fa60`) are supported.
Settings are stored in the mouse, so nothing needs to keep running.

## Install

```sh
cargo install --path .

# let your user open the mouse without root (once)
x3ctl udev-rule | sudo tee /etc/udev/rules.d/70-x3ctl.rules
sudo udevadm control --reload
sudo udevadm trigger --settle --subsystem-match=hidraw
```

`x3ctl list` should then show the mouse with `ok` in the last column.

## Use

```sh
x3ctl read                                # what the mouse holds now
x3ctl set --dpi 1600                      # DPI of the active stage
x3ctl set --dpis 400,800,1600,3200,6400,12800 --stage 3
x3ctl set --dpi2 800                      # one specific stage
x3ctl set --polling 1000
x3ctl set --lod 1 --motion-sync on --angle-snap off --ripple off
x3ctl set --sleep 2 --deep-sleep 10 --debounce 4
x3ctl show                                # x3ctl's saved copy
x3ctl apply                               # write the saved copy back
x3ctl battery                             # e.g. "70% (on battery)", dongle only
x3ctl monitor                             # DPI-button and battery events
x3ctl -n set --dpi 1600                   # dry run: print the bytes, send nothing
```

`-d wired`, `-d dongle` or `-d /dev/hidrawN` picks a device when more than one
is connected; the default is the cable. `-v` shows the bytes exchanged.

### How a change is applied

The mouse takes whole blocks of settings at a time: all six DPI stages and the
sensor flags travel in one report, the timers in another. So `set` first reads
the block from the mouse, changes only what you asked for, writes it back and
waits for the mouse to acknowledge it. Whatever you did not mention stays as
the mouse had it.

Each successful `read --save` or `set` also stores a copy in
`~/.config/x3ctl/settings.conf`. `x3ctl apply` writes that copy back, which
makes it usable as a profile, and `set` falls back to it if the mouse cannot
be read.

## Status

Tested on a wired X3 (`1d57:fa61`, USB device version 1.11): reading, writing, and the
mouse's acknowledgement of each write all work, and values read back match
what was written. The report encoders are also tested byte-for-byte against
USB captures of the official Windows software (`cargo test`).

Not yet tested against a mouse that is actually on the 2.4G link:

- **Writing through the dongle.** Reports 0x04/0x05 are zero-padded to 56/15
  bytes there, as drivers for sibling models do. A write that the mouse does
  not acknowledge is reported as an error, not as success.
- **`x3ctl battery`**, which only the dongle reports.

The X3 shares its USB ids with the Attack Shark R1, which lays out report 0x04
differently. Do not use this on an R1.

Not implemented: button remapping, macros, the DPI indicator colours.

## Protocol

All configuration goes through HID feature reports on interface 2.

| report | wire bytes | contents |
|--------|-----------|----------|
| `0x04` | 52 | lift-off `[3]`, ripple `[4]`, angle snap `[6]`, motion sync `[7]`, DPI low bytes `[8..14]`, DPI high bytes `[16..22]`, active stage `[24]`, stage colours `[25..49]` |
| `0x05` | 13 | deep sleep (12 bits across `[4]` high nibble and `[5]`: minutes × 16 + 8), sleep `[9]` in half minutes, debounce `[10]` in 2 ms |
| `0x06` | 9 | polling code `[3]` = 1000 / Hz, complement in `[4]` |

A DPI is stored as `dpi / 50 - 1` in 16 bits. Reports 0x04 and 0x05 start with
`id, length, 0x01` and end with the big-endian 16-bit sum of the bytes in
between.

Input report `0x03` carries events: `10` DPI stage changed, `40` battery
(state, level in tenths), `50` write processed (status, report id; status 0
is success).

To read a report back, send feature report `a0 <id> <length> 00 01 00 00 00`,
wait for a GET of `0xa0` to return `a0 01`, then GET the report itself.

## Credits

The X3 byte layouts come from the captures and analysis in
[DavBfr/shark-x3](https://github.com/DavBfr/shark-x3). The read handshake and
event codes follow
[HarukaYamamoto0/attack-shark-x11-driver](https://github.com/HarukaYamamoto0/attack-shark-x11-driver)
and [cyberps96/attack-shark-ctl](https://github.com/cyberps96/attack-shark-ctl);
[xb-bx/attack-shark-r1-driver](https://github.com/xb-bx/attack-shark-r1-driver)
was the starting point.
