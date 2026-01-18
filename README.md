Download Eventide and Meris patches from patchstorage.

Supports:
* Eventide H90
* Meris Enzo X
* Meris LVX
* Meris MercuryX
* Bram Bos Mozaic
* Empress ZOIA

```shell
cargo build && cargo test
cargo run -- -h
cargo run -- -o out -p meris-enzo-x
```

References:
* [Patchstorage endpoint](https://patchstorage.com/docs/api/beta/)
* [Wiki](https://github.com/patchstorage/patchstorage-docs/wiki)

Adapted from https://github.com/meanmedianmoge/zoia_lib
