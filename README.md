# imge

Write disk images to physical drive.

## Install

```
$ cargo install imge
$ export PATH=$PATH:~/.cargo/bin
```

## Synopsis

```
imge <image> [-a] [-m]

Positional Arguments:
  image             path to image

Options:
  -a, --all-drives  show all drives
  -m, --magenta     magenta mode
  --help            display usage information
```

## Description

`Imge` is a TUI tool for writing disk images to removable (by default) or non-removable (by `-a` option) drives. It's intended to be an easier to use and less error-prone than `dd`, since choosing the wrong disk may have a big impact on the data on your hard drive.

![main](screenshots/1-main.avif)
![keybindings](screenshots/2-keybindings.avif)
![warning](screenshots/3-warning.avif)
![progress](screenshots/4-progress.avif)
![victory](screenshots/5-victory.avif)

## TODO

- Add ability to copy drive to file too.
- Compress/decompress image on the fly.
- Verify if data was copied correctly.
- Verify checksum before making copy.
- Support copying /dev/zero and /dev/urandom to the drive.
- Implement non-interactive mode.
