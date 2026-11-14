# qdl - Sahara / Firehose tools, in Rust!

Qualcomm SoCs feature the Emergency Download Mode (EDL, widely known as '9008 mode'), a bootrom-initiated device flashing stack.

`qdl` provides a Rust implementation for the Sahara and Firehose protocols that are used to communicate with a device in that mode.

## Oplus EDL fork

This fork starts at Qualcomm commit `75fe2f7e42546a1d75969040378d4616419a9dcf`.
It contains the first protocol fixes for Oplus EDL. The workspace still contains the upstream tools and retains the BSD-3-Clause license.
The tests use scripted transport. They do not open USB devices or serial ports.
Offline success does not establish device support.

`firehose_program_storage`, `firehose_patch`, `firehose_set_bootable`, and `firehose_reset` return `MutationFailure` on failure.
Its `confirmation` field is `NotSent`, `Rejected`, or `Unknown`. Its `cause` field retains the original error.
A complete NAK means `Rejected`. A transport failure after sending starts means `Unknown`.
Success requires the final ACK. Callers must stop after failure and must not automatically retry modifications.
Neither errors nor `QdlDevice` destruction send reset commands. The retained `reset_on_drop` field has no effect.
Call `firehose_reset` explicitly when the workflow permits a reset.

The program input must supply all requested sector bytes. The library no longer pads early EOF with buffer contents.
Callers that permit a partial final sector must provide that padding explicitly.
The upstream CLI uses a bounded zero stream for erase and pads only the final image sector.
Rawprogram file offsets and lengths are checked before that program command is sent.
A file that shrinks after this check still fails; it is not padded to hide missing source bytes.
Serial writes can continue after a short write. USB writes require a complete transfer and reject short completions.
USB cancellation becomes `TimedOut`, because cancellation can occur after bytes reach the device.
Storage reads require complete output writes, a successful output flush, and a final ACK.
Only a complete NAK before raw data, without retry/restart hints or a raw-mode indication, produces `ReadRejected`.
An absent `rawmode` attribute or `rawmode="false"` permits this classification. Other read failures require stopping the session.

`firehose_read_welcome` collects startup logs. An idle timeout ends welcome collection only at an XML document boundary.
XML reads skip framing whitespace before the next document, including whitespace left by the welcome end marker.
They return immediately at the document end, preserving raw storage bytes after an ACK. Partial XML still fails on timeout.
`firehose_read` requires a complete response, even after a log or partial XML.
`firehose_get_storage_info` returns log messages and response attributes, and reports NAK as an error.
Configuration responses update sizes only after a valid ACK. They do not send another configure or reset command.
Payload sizes must be nonzero sector multiples. Payload and XML buffers have a 16 MiB limit.
`bypass_storage` defaults to false. The existing explicit throughput-testing setting remains available.

Use `list_usb_device_keys` with `setup_usb_device_by_key` to select a physical USB location.
`setup_usb_device_at` uses the same selection check. The legacy USB entry point requires an exact serial-number match.
Serial selection requires an enumerated port path. Missing, absent, or duplicate selections never open a device.
Set `FirehoseConfiguration::quiet` to suppress logs and progress on the loader and storage paths.
The upstream CLI keeps its own output and successful-run reset policy. It is not the Oplus EDL product CLI.

Run the offline checks from the fork directory:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --locked
cargo build --workspace --all-features --locked
```

The integration suites are `firehose_io`, `sahara_transfer`, `selection`, and the CLI's `program_input`.
The CLI suite exercises the production input adapters and rawprogram parser with temporary files and scripted transport.
They cover local file faults separately from transport faults. Command records remain available after the session is dropped.
Product workflows and their stop and verification policies belong to later implementation stages.

## Contents
```
cli/ - A CLI tool used to communicate with devices in EDL mode
qdl/ - Sahara / Firehose library, with USB convenience wrappers
qramdump/ - Tool to receive memory dumps from a crashed device
```

## Building
You're expected to have a recent installation of Rust. You can acquire one with [rustup](https://rustup.rs).
</br>If you already have an older installation, try `rustup update`.

Run `cargo build [--release]` to build all executables within this repo. The binaries will appear in `target/debug/` or `target/release`, respectively.

Use `cargo run [--release] --bin <qdl-rs/qramdump> [-- args]` to quickly build one of the programs from source and run it.

## Running the programs
<details>

<summary>qdl-rs</summary>

```
Usage: qdl-rs [OPTIONS] --loader-path <FILE> --storage-type <emmc/ufs/nvme/nand> <COMMAND>

Commands:
  dump               Dump the entire storage
  dump-part          Dump a single partition
  flasher            Invoke the flasher
  erase              Erase a partition
  nop                Ask the device to do nothing, hopefully successfully
  overwrite-storage  Overwrite the storage physical partition contents with a raw image Similar to Flasher, but this one only takes a partition dump as input and performs no real validation on the input data
  peek               Peek at memory
  print-gpt          Print the GPT table
  reset              Restart the device
  set-bootable-part  Mark physical storage partition as bootable
  write              Write a partition
  help               Print this message or the help of the given subcommand(s)

Options:
      --backend <usb/serial>

      --bypass-storage
          Accept storage r/w operations, but make them never actually execute (useful for testing USB throughput)
  -d, --dev-path <DEV_PATH>
          E.g. COM4 on Windows
  -l, --loader-path <FILE>

      --hash-packets
          Validate every packet. Slow.
  -L, --phys-part-idx <PHYS_PART_IDX>
          e.g. LUN index for UFS [default: 0]
      --print-firehose-log

      --read-back-verify
          Every <program> operation is read back. VERY SLOW!
      --reset-mode <edl/off/system>
          WARNING: Will be deprecated in release v1.0.0 [default: edl]
      --serial-no <SERIAL_NO>

  -A, --skip-hello-wait
          Work around missing HELLO packet
  -s, --storage-type <emmc/ufs/nvme/nand>

  -S, --storage-slot <STORAGE_SLOT>
          Index of the physical device (e.g. 1 for secondary UFS) [default: 0]
      --sector-size <SECTOR_SIZE>

      --skip-storage-init
          Required for unprovisioned storage media.
      --verbose-sahara

      --verbose-firehose

  -h, --help
          Print help
  -V, --version
          Print version
```

</details>

<details>

<summary>qramdump</summary>

```
Usage: qramdump [OPTIONS] [REGIONS_TO_DUMP]...

Arguments:
  [REGIONS_TO_DUMP]...

Options:
      --backend <usb/serial>
  -d, --dev-path <DEV_PATH>    E.g. COM4 on Windows
      --serial-no <SERIAL_NO>
      --verbose-sahara
  -h, --help                   Print help
  -V, --version                Print version
```

</details>

### Windows
You'll need to acquire an appropriate driver that exposes the device as a USB serial port, or use [WinUSB](https://learn.microsoft.com/en-us/windows-hardware/drivers/usbcon/winusb-installation).

Serial is used as the default backend on this platform.

### Loader filename on newer platforms
Some newer platforms (e.g. SM8750) require that a file called `xbl_s_devprg_ns.melf` is used instead of `prog_firehose_ddr.elf`. This change may be opaque to you if the file has been renamed as part of the binary delivery process.

### LUN handling
Due to how the protocol is constructed, particularly when interfacing with UFS, you ***must*** specify the LUN (physical storage partition) index on which you want to operate. This does not concern the `flasher` command (rawproramN.xml files include that information) and operations that aren't storage-related (e.g. `peek` or `nop`).

## Common usage examples

<details>
<summary>Flash a full META image</summary>
  
### Example with UFS as primary storage, reboots to OS after flashing ends
```
qdl-rs --serial-no <SERIAL_NO> -l prog_firehose_ddr.elf -s ufs --reset-mode system flasher -p rawprogram*.xml -x patch*.xml

# NOTE: qdl-rs will flash anything you pass as a parameter. Some METAs ship a number of rawprogram0_foo.xml
# files which may be undesirable (e.g. _WIPE_GPT). You can filter those out with e.g.:
find /path/to/build/ -regex '.*/rawprogram[0-9]+\.xml$'
```

</details>

<details>
<summary>Reboot the device back to the OS</summary>

```
qdl-rs --serial-no <SERIAL_NO> -l prog_firehose_ddr.elf -s ufs reset system
```

</details>

<details>
<summary>Dump the entire physical storage partition (e.g. LUN)</summary>
  
```
qdl-rs --serial-no <SERIAL_NO> -l prog_firehose_ddr.elf -s ufs --phys-part-idx 2 dump -o lun2/
```

</details>

<details>
<summary>Fetch a single partition from LUN2</summary>

```
qdl-rs --serial-no <SERIAL_NO> -l prog_firehose_ddr.elf -s ufs --phys-part-idx 2 dump-part EFI
```

</details>

<details>
<summary>Overwrite a single partition on LUN0</summary>

```
qdl-rs --serial-no <SERIAL_NO> -l prog_firehose_ddr.elf -s ufs --phys-part-idx 0 write boot boot.img
```

</details>

<details>
<summary>Print out the partition table on LUN4</summary>

```
qdl-rs --serial-no <SERIAL_NO> -l prog_firehose_ddr.elf -s ufs --phys-part-idx 4 print-gpt
```

</details>

<details>
<summary>Overwrite the entirety of LUN7 (VERY dangerous, may remove device-unique data)</summary>

```
qdl-rs --serial-no <SERIAL_NO> -l prog_firehose_ddr.elf -s ufs --phys-part-idx 7 overwrite-storage lun7_dump.img
```

</details>

<details>
<summary>Erase a partition on eMMC (VERY dangerous, may remove device-unique data)</summary>

```
qdl-rs --serial-no <SERIAL_NO> -l prog_firehose_ddr.elf -s emmc erase boot
```

</details>

<details>
<summary>Set LUN2 as bootable (i.e. containing xbl)</summary>

```
qdl-rs --serial-no <SERIAL_NO> -l prog_firehose_ddr.elf -s ufs set-bootable-part 2
```

</details>

## Documentation

Run `cargo doc --open` to generate and open the latest rustdoc. Learn more [here](https://doc.rust-lang.org/cargo/commands/cargo-doc.html).

## Contributing

See [`CONTRIBUTING.md`](/CONTRIBUTING.md).
Your code is expected to pass `cargo fmt` and `cargo clippy` checks - the CI will be on the lookout for that.

## License

See the [`LICENSE` file](/LICENSE).

## Credits

* [linux-msm/qdl](https://github.com/linux-msm/qdl) for the open C implementation
