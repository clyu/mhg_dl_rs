# mhg_dl_rs

This repository is a Rust-based re-implementation of the `manhuagui-dlr` project (https://github.com/HSSLC/manhuagui-dlr). The majority of this rewrite was facilitated by OpenAI's o4-mini model.

## Usage

To use the program, follow the command-line interface below:

```
USAGE:
    mhg_dl_rs.exe [OPTIONS] <URL>

ARGS:
    <URL>    Manhuagui URL or numeric ID

OPTIONS:
    -d, --delay-ms <DELAY_MS>        Delay between pages in milliseconds [default: 1000]
    -h, --help                       Print help information
    -o, --output-dir <OUTPUT_DIR>    Output directory [default: Downloads]
    -t, --tunnel <TUNNEL>            Tunnel line: 0=i,1=eu,2=us [default: 0]
    -V, --version                    Print version information
```

## Citation

If you utilize this project in your work, please consider citing both the original `manhuagui-dlr` project and this `mhg_dl_rs` repository.
