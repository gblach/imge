# imge

Write disk images to physical drive or vice versa with on-the-fly compression/decompression.

## Install

```
# Install from source
$ cargo install imge

# Install from binary
$ cargo binstall imge

# If ~/.cargo/bin is not in your PATH
$ export PATH=$PATH:~/.cargo/bin
```

## Synopsis

```
imge <image> [-a] [-d <drive>] [-f] [-v]

Positional Arguments:
  image             path to image

Options:
  -a, --all-drives  show all drives
  -d, --drive       use this drive, do not ask
  -f, --from-drive  copy drive to image (instead of image to drive)
  -v, --verify      verify if data was copied correctly
  --help            display usage information
```

## Description

`Imge` is a TUI tool for writing disk images to removable (by default) or non-removable
(by `-a` option) drives. It also has an option to copy the drive to the disk image.
When copying from image to disk and the image is compressed, the image is decompressed on the fly.
When copying from disk to image and the image ends in .gz, .bz2 or .xz,
the image is compressed on the fly.
It's intended to be an easier to use and less error-prone than `dd`,
since choosing the wrong disk may have a big impact on the data on your hard drive.

![main](https://raw.githubusercontent.com/gblach/imge/e9ac4a0/screenshots/1-main.avif)
![keybindings](https://raw.githubusercontent.com/gblach/imge/e9ac4a0/screenshots/2-keybindings.avif)
![warning](https://raw.githubusercontent.com/gblach/imge/e9ac4a0/screenshots/3-warning.avif)
![copying](https://raw.githubusercontent.com/gblach/imge/e9ac4a0/screenshots/4-copying.avif)
![verifying](https://raw.githubusercontent.com/gblach/imge/e9ac4a0/screenshots/5-verifying.avif)
![victory](https://raw.githubusercontent.com/gblach/imge/e9ac4a0/screenshots/6-victory.avif)

## TODO

- Verify checksum before making copy.
- Support copying /dev/zero and /dev/urandom to the drive.
