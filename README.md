Setup and run
--------------

```
cargo run -- -a certs/ -c config/uplink.toml -i 1 -vv
```

Testing with netcat
--------------

* Send content of a text file

```
nc localhost 5555 < tools/netcat.txt
```

* Send json one by one

```
nc localhost 5555
{ "stream": "can", "sequence": 1, "timestamp": 12345, "data": 100 }
{ "stream": "can", "sequence": 1, "timestamp": 12345, "data": 100 }
{ "stream": "can", "sequence": 1, "timestamp": 12345, "data": 100 }
```

Build for Beagle
--------------
Install arm compilers and linkers

```
apt install gcc-9-arm-linux-gnueabihf
ln -s /usr/bin/arm-linux-gnueabihf-gcc-9 /usr/bin/arm-linux-gnueabihf-gcc
```
create `.cargo/config`

```
[target.armv7-unknown-linux-gnueabihf]
linker = "arm-linux-gnueabihf-gcc"

[build]
rustflags = ["-C", "rpath"]
```

```
rustup target install armv7-unknown-linux-gnueabihf
cargo build --release --target armv7-unknown-linux-gnueabihf
```

References
----------
* [Rust target list to arm architecture map](https://forge.rust-lang.org/release/platform-support.html)
* [Arm architectures](https://en.wikipedia.org/wiki/List_of_ARM_microarchitectures)
* https://users.rust-lang.org/t/how-to-pass-cargo-linker-args/3163/2 
* https://sigmaris.info/blog/2019/02/cross-compiling-rust-on-mac-os-for-an-arm-linux-router/
